//! A table's columns out of one Parquet file, a batch of rows at a time.
//! Only the columns asked for are read, each as the manifest's type for
//! it: a file's own types say how a value is stored, not what PostgreSQL
//! would hold.

use std::fs::File;
use std::path::Path;

use parquet::basic::{ConvertedType, LogicalType, TimeUnit, Type as Stored};
use parquet::column::reader::{ColumnReader, ColumnReaderImpl};
use parquet::data_type::{BoolType, ByteArrayType, DataType, DoubleType, Int32Type, Int64Type};
use parquet::errors::ParquetError;
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::schema::types::ColumnDescriptor;
use straum_domain::{ColumnType, Datum};

use crate::manifest::Column;
use crate::value::{self, Zone};

/// Rows per batch handed on. Straum starts a thread per wider bucket
/// width for each batch it adds, so a small one costs.
const BATCH: usize = 8192;

/// How a file's column becomes the manifest's.
enum Reading {
    Int,
    /// An integer as a numeric of this scale.
    Whole {
        scale: u32,
    },
    /// A double as a numeric of this scale.
    Double {
        scale: u32,
    },
    Bool,
    Text,
    /// A count of parts of a second: since 1970 in UTC for a
    /// `timestamptz`, on the wall clock for a `timestamp`.
    Seconds {
        per_second: i64,
    },
    /// A count of parts of a second on a zone's wall clock, read as the
    /// instant the clock showed it.
    Clock {
        per_second: i64,
        zone: Zone,
    },
}

/// One column asked for, and its values of the batch being read.
struct Lane<'a> {
    column: &'a Column,
    /// Where the column is in the file.
    index: usize,
    /// The definition level of a value that is there; 0 when none may be
    /// null.
    present: i16,
    reading: Reading,
    values: Vec<Datum>,
}

/// Buffers every lane reads through.
#[derive(Default)]
struct Scratch {
    levels: Vec<i16>,
    int32s: Vec<i32>,
    int64s: Vec<i64>,
    doubles: Vec<f64>,
    bools: Vec<bool>,
    bytes: Vec<parquet::data_type::ByteArray>,
    text: String,
}

/// What stops a batch: the file, or a value at this place in the batch.
enum Stopped {
    File(ParquetError),
    Value { at: usize, shown: String },
}

impl From<ParquetError> for Stopped {
    fn from(e: ParquetError) -> Stopped {
        Stopped::File(e)
    }
}

/// Reads `columns` of every row of the file at `path` and calls `each`
/// with the rows of each batch, laid end to end. The rows read. An error
/// says what is wrong with the file, or is `each`'s own.
pub(crate) fn read<E: From<String>>(
    path: &Path,
    columns: &[&Column],
    mut each: impl FnMut(&[Datum]) -> Result<(), E>,
) -> Result<u64, E> {
    let named = |what: String| E::from(format!("{}: {what}", path.display()));
    let file = File::open(path).map_err(|e| named(e.to_string()))?;
    let reader = SerializedFileReader::new(file).map_err(|e| named(e.to_string()))?;
    let schema = reader.metadata().file_metadata().schema_descr();
    let mut lanes = Vec::new();
    for column in columns {
        let lane = lane(column, schema.columns()).map_err(named)?;
        lanes.push(lane);
    }
    let mut scratch = Scratch::default();
    let mut rows = Vec::new();
    let mut total = 0;
    for group in 0..reader.num_row_groups() {
        let group = reader
            .get_row_group(group)
            .map_err(|e| named(e.to_string()))?;
        let mut readers = Vec::new();
        for lane in &lanes {
            let reader = group.get_column_reader(lane.index);
            readers.push(reader.map_err(|e| named(e.to_string()))?);
        }
        loop {
            let mut read = None;
            for (lane, reader) in lanes.iter_mut().zip(&mut readers) {
                lane.values.clear();
                let count = match batch(lane, reader, &mut scratch) {
                    Ok(count) => count,
                    Err(Stopped::File(e)) => return Err(named(e.to_string())),
                    Err(Stopped::Value { at, shown }) => {
                        return Err(named(format!(
                            "row {}: can't read {shown} as {} in column {}",
                            total + at as u64 + 1,
                            lane.column.column_type,
                            lane.column.name
                        )));
                    }
                };
                if read.is_some_and(|read| read != count) {
                    return Err(named("its columns hold different numbers of rows".into()));
                }
                read = Some(count);
            }
            // No columns asked for is no rows to give.
            let count = read.unwrap_or(0);
            if count == 0 {
                break;
            }
            rows.clear();
            for row in 0..count {
                rows.extend(lanes.iter().map(|lane| lane.values[row].clone()));
            }
            each(&rows)?;
            total += count as u64;
        }
    }
    Ok(total)
}

/// Finds the file's column for a manifest column and how to read it.
fn lane<'a>(
    column: &'a Column,
    stored: &[std::sync::Arc<ColumnDescriptor>],
) -> Result<Lane<'a>, String> {
    // A column inside a group or a list is no column of a table.
    let top = |descr: &ColumnDescriptor| {
        descr.max_rep_level() == 0 && descr.path().parts() == [column.from.as_str()]
    };
    let Some(index) = stored.iter().position(|descr| top(descr)) else {
        return Err(format!(
            "has no column {}, which column {} is read from",
            column.from, column.name
        ));
    };
    let descr = &stored[index];
    let reading = reading(column, descr).map_err(|why| {
        format!(
            "column {} is read from {}, which {why}",
            column.name, column.from
        )
    })?;
    Ok(Lane {
        column,
        index,
        present: descr.max_def_level(),
        reading,
        values: Vec::new(),
    })
}

/// The parts of a second a timestamp column counts, and whether it
/// counts them from 1970 in UTC; `None` for a column that is no
/// timestamp.
fn timestamp(descr: &ColumnDescriptor) -> Option<(i64, bool)> {
    if let Some(LogicalType::Timestamp(stamp)) = descr.logical_type_ref() {
        let per_second = match stamp.unit {
            TimeUnit::MILLIS => 1_000,
            TimeUnit::MICROS => 1_000_000,
            TimeUnit::NANOS => 1_000_000_000,
        };
        return Some((per_second, stamp.is_adjusted_to_u_t_c));
    }
    // Before files said which, a timestamp was an instant.
    match descr.converted_type() {
        ConvertedType::TIMESTAMP_MILLIS => Some((1_000, true)),
        ConvertedType::TIMESTAMP_MICROS => Some((1_000_000, true)),
        _ => None,
    }
}

fn reading(column: &Column, descr: &ColumnDescriptor) -> Result<Reading, String> {
    let stored = descr.physical_type();
    let integer = matches!(stored, Stored::INT32 | Stored::INT64);
    let wanted = column.column_type;
    let mismatch = || format!("is {stored} in the file and doesn't read as {wanted}");
    match wanted {
        ColumnType::Int if integer => Ok(Reading::Int),
        ColumnType::Numeric { scale } if integer => Ok(Reading::Whole { scale }),
        ColumnType::Numeric { scale } if stored == Stored::DOUBLE => Ok(Reading::Double { scale }),
        ColumnType::Bool if stored == Stored::BOOLEAN => Ok(Reading::Bool),
        ColumnType::Text if stored == Stored::BYTE_ARRAY => Ok(Reading::Text),
        ColumnType::Time { zoned } if stored == Stored::INT64 => {
            let (per_second, instant) = timestamp(descr).ok_or_else(mismatch)?;
            match (zoned, instant, &column.zone) {
                (true, true, None) | (false, false, _) => Ok(Reading::Seconds { per_second }),
                (true, true, Some(_)) => Err("is an instant in the file; drop the in ZONE".into()),
                (true, false, None) => {
                    Err("is a clock with no zone in the file; say whose with in ZONE".into())
                }
                (true, false, Some(name)) => {
                    let zone = Zone::named(name)
                        .ok_or_else(|| format!("is read in {name}, which is no time zone"))?;
                    Ok(Reading::Clock { per_second, zone })
                }
                (false, true, _) => Err("is an instant in the file; write timestamptz".into()),
            }
        }
        _ => Err(mismatch()),
    }
}

/// Reads the lane's next batch into its values; how many rows it was.
fn batch(
    lane: &mut Lane,
    reader: &mut ColumnReader,
    scratch: &mut Scratch,
) -> Result<usize, Stopped> {
    let Scratch {
        levels,
        int32s,
        int64s,
        doubles,
        bools,
        bytes,
        text,
    } = scratch;
    let (present, out) = (lane.present, &mut lane.values);
    let int = |n: i64| Some(Datum::Int(n));
    match (reader, &mut lane.reading) {
        (ColumnReader::Int32ColumnReader(r), Reading::Int) => {
            typed::<Int32Type>(r, present, levels, int32s, out, |v| int((*v).into()), plain)
        }
        (ColumnReader::Int64ColumnReader(r), Reading::Int) => {
            typed::<Int64Type>(r, present, levels, int64s, out, |v| int(*v), plain)
        }
        (ColumnReader::Int32ColumnReader(r), Reading::Whole { scale }) => {
            let read = |v: &i32| value::whole((*v).into(), *scale).and_then(int);
            typed::<Int32Type>(r, present, levels, int32s, out, read, plain)
        }
        (ColumnReader::Int64ColumnReader(r), Reading::Whole { scale }) => {
            let read = |v: &i64| value::whole(*v, *scale).and_then(int);
            typed::<Int64Type>(r, present, levels, int64s, out, read, plain)
        }
        (ColumnReader::DoubleColumnReader(r), Reading::Double { scale }) => {
            let read = |v: &f64| value::numeric(*v, *scale, text).and_then(int);
            typed::<DoubleType>(r, present, levels, doubles, out, read, plain)
        }
        (ColumnReader::BoolColumnReader(r), Reading::Bool) => typed::<BoolType>(
            r,
            present,
            levels,
            bools,
            out,
            |v| Some(Datum::Bool(*v)),
            plain,
        ),
        (ColumnReader::ByteArrayColumnReader(r), Reading::Text) => {
            let read = |v: &parquet::data_type::ByteArray| {
                str::from_utf8(v.data())
                    .ok()
                    .map(|text| Datum::Text(text.into()))
            };
            typed::<ByteArrayType>(r, present, levels, bytes, out, read, lossy)
        }
        (ColumnReader::Int64ColumnReader(r), Reading::Seconds { per_second }) => {
            let read = |v: &i64| int(value::seconds(*v, *per_second));
            typed::<Int64Type>(r, present, levels, int64s, out, read, plain)
        }
        (ColumnReader::Int64ColumnReader(r), Reading::Clock { per_second, zone }) => {
            let read = |v: &i64| zone.instant(value::seconds(*v, *per_second)).and_then(int);
            typed::<Int64Type>(r, present, levels, int64s, out, read, plain)
        }
        // A row group stores a column as its file's schema says.
        _ => Err(Stopped::File(ParquetError::General(
            "a row group that is not of its file's schema".into(),
        ))),
    }
}

fn plain<V: std::fmt::Display>(value: &V) -> String {
    value.to_string()
}

/// Bytes that are no UTF-8, as near to text as they get.
fn lossy(value: &parquet::data_type::ByteArray) -> String {
    format!("{:?}", String::from_utf8_lossy(value.data()))
}

/// Reads a batch of one stored type: nulls where the levels say so, and
/// every other value through `read`, which gives `None` for one that is
/// no value of the manifest's type; `show` prints that one.
fn typed<T: DataType>(
    reader: &mut ColumnReaderImpl<T>,
    present: i16,
    levels: &mut Vec<i16>,
    stored: &mut Vec<T::T>,
    out: &mut Vec<Datum>,
    mut read: impl FnMut(&T::T) -> Option<Datum>,
    show: fn(&T::T) -> String,
) -> Result<usize, Stopped> {
    levels.clear();
    stored.clear();
    let (rows, _, _) = reader.read_records(BATCH, Some(levels), None, stored)?;
    let mut stored = stored.iter();
    for at in 0..rows {
        // A column that holds no nulls has no levels.
        if present > 0 && levels.get(at).is_none_or(|level| *level < present) {
            out.push(Datum::Null);
            continue;
        }
        let value = stored.next().ok_or_else(|| {
            ParquetError::General("a column with fewer values than its levels say".into())
        })?;
        out.push(read(value).ok_or_else(|| Stopped::Value {
            at,
            shown: show(value),
        })?);
    }
    Ok(rows)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use parquet::basic::{Compression, ZstdLevel};
    use parquet::data_type::ByteArray;
    use parquet::file::properties::WriterProperties;
    use parquet::file::writer::{SerializedColumnWriter, SerializedFileWriter};
    use parquet::schema::parser::parse_message_type;
    use straum_domain::Identifier;

    use super::*;

    /// A directory of the test's own, removed at the end.
    pub(crate) struct Scratch(pub(crate) PathBuf);

    static SCRATCH: AtomicUsize = AtomicUsize::new(0);

    impl Scratch {
        pub(crate) fn new() -> Scratch {
            let n = SCRATCH.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("sp-unit-{}-{n}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Scratch(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    /// A column of one row group, as it is stored.
    pub(crate) enum Cells {
        Int32(Vec<Option<i32>>),
        Int64(Vec<Option<i64>>),
        Double(Vec<Option<f64>>),
        Bool(Vec<Option<bool>>),
        Bytes(Vec<Option<&'static [u8]>>),
        /// A column that may hold no null, so stores no levels.
        Required(Vec<i64>),
    }

    fn put<T: DataType>(writer: &mut SerializedColumnWriter, cells: &[Option<T::T>])
    where
        T::T: Clone,
    {
        let levels: Vec<i16> = cells.iter().map(|cell| cell.is_some().into()).collect();
        let stored: Vec<T::T> = cells.iter().flatten().cloned().collect();
        let typed = writer.typed::<T>();
        typed.write_batch(&stored, Some(&levels), None).unwrap();
    }

    /// Writes a file of `schema` with a row group per item of `groups`.
    pub(crate) fn write(
        path: &Path,
        schema: &str,
        compression: Compression,
        groups: Vec<Vec<Cells>>,
    ) {
        let schema = Arc::new(parse_message_type(schema).unwrap());
        let properties = WriterProperties::builder()
            .set_compression(compression)
            .build();
        let file = File::create(path).unwrap();
        let mut writer = SerializedFileWriter::new(file, schema, Arc::new(properties)).unwrap();
        for group in groups {
            let mut rows = writer.next_row_group().unwrap();
            for cells in group {
                let mut column = rows.next_column().unwrap().unwrap();
                match cells {
                    Cells::Int32(cells) => put::<Int32Type>(&mut column, &cells),
                    Cells::Int64(cells) => put::<Int64Type>(&mut column, &cells),
                    Cells::Double(cells) => put::<DoubleType>(&mut column, &cells),
                    Cells::Bool(cells) => put::<BoolType>(&mut column, &cells),
                    Cells::Bytes(cells) => {
                        let cells: Vec<Option<ByteArray>> = (cells.iter())
                            .map(|cell| cell.map(|bytes| ByteArray::from(bytes.to_vec())))
                            .collect();
                        put::<ByteArrayType>(&mut column, &cells);
                    }
                    Cells::Required(cells) => {
                        let typed = column.typed::<Int64Type>();
                        typed.write_batch(&cells, None, None).unwrap();
                    }
                }
                column.close().unwrap();
            }
            rows.close().unwrap();
        }
        writer.close().unwrap();
    }

    pub(crate) fn column(
        name: &str,
        column_type: ColumnType,
        from: &str,
        zone: Option<&str>,
    ) -> Column {
        Column {
            name: Identifier::new(name).unwrap(),
            column_type,
            from: from.to_owned(),
            zone: zone.map(str::to_owned),
        }
    }

    const TRIPS: &str = "message schema {
        OPTIONAL INT32 VendorID;
        OPTIONAL INT64 tpep_pickup_datetime (TIMESTAMP(MICROS,false));
        OPTIONAL INT64 payment_type;
        OPTIONAL DOUBLE fare_amount;
        OPTIONAL BYTE_ARRAY store_and_fwd_flag (STRING);
        OPTIONAL BOOLEAN shared;
        OPTIONAL INT64 seen (TIMESTAMP(MILLIS,true));
        REQUIRED INT64 id;
    }";

    const TIMESTAMPTZ: ColumnType = ColumnType::Time { zoned: true };
    const TIMESTAMP: ColumnType = ColumnType::Time { zoned: false };
    const FARE: ColumnType = ColumnType::Numeric { scale: 2 };

    /// 2024-01-15 10:30 on a wall clock, in microseconds; 15:30 UTC when
    /// the clock is New York's.
    const MORNING: i64 = 1_705_314_600_000_000;

    /// Two row groups of the trips: three rows, then one.
    fn trips(path: &Path, compression: Compression) {
        let first = vec![
            Cells::Int32(vec![Some(1), None, Some(2)]),
            Cells::Int64(vec![Some(MORNING), Some(MORNING + 999_999), None]),
            Cells::Int64(vec![Some(1), Some(2), None]),
            Cells::Double(vec![Some(12.5), None, Some(-0.05)]),
            Cells::Bytes(vec![Some(b"N"), Some(b""), None]),
            Cells::Bool(vec![Some(true), None, Some(false)]),
            Cells::Int64(vec![Some(1_500), None, Some(-1)]),
            Cells::Required(vec![7, 8, 9]),
        ];
        let second = vec![
            Cells::Int32(vec![Some(6)]),
            Cells::Int64(vec![Some(0)]),
            Cells::Int64(vec![Some(4)]),
            Cells::Double(vec![Some(1.005)]),
            Cells::Bytes(vec![Some("Ø".as_bytes())]),
            Cells::Bool(vec![Some(true)]),
            Cells::Int64(vec![Some(0)]),
            Cells::Required(vec![10]),
        ];
        write(path, TRIPS, compression, vec![first, second]);
    }

    fn rows(path: &Path, columns: &[Column]) -> Result<Vec<Vec<Datum>>, String> {
        let columns: Vec<&Column> = columns.iter().collect();
        let mut rows = Vec::new();
        let total = read(path, &columns, |batch: &[Datum]| {
            rows.extend(batch.chunks(columns.len()).map(<[Datum]>::to_vec));
            Ok::<(), String>(())
        })?;
        assert_eq!(total, rows.len() as u64);
        Ok(rows)
    }

    fn text(value: &str) -> Datum {
        Datum::Text(value.into())
    }

    #[test]
    fn reads_the_columns_asked_for_as_the_manifest_types_them() {
        let scratch = Scratch::new();
        let path = scratch.0.join("trips.parquet");
        let columns = [
            column("fare_amount", FARE, "fare_amount", None),
            column(
                "pickup_at",
                TIMESTAMPTZ,
                "tpep_pickup_datetime",
                Some("America/New_York"),
            ),
            column("vendor_id", ColumnType::Int, "VendorID", None),
            column("payment_type", ColumnType::Int, "payment_type", None),
            column("flag", ColumnType::Text, "store_and_fwd_flag", None),
            column("shared", ColumnType::Bool, "shared", None),
            column("seen", TIMESTAMPTZ, "seen", None),
            column("id", ColumnType::Int, "id", None),
        ];
        let int = Datum::Int;
        let (null, yes, no) = (Datum::Null, Datum::Bool(true), Datum::Bool(false));
        let expected = [
            vec![
                int(1_250),
                int(1_705_332_600),
                int(1),
                int(1),
                text("N"),
                yes.clone(),
                int(1),
                int(7),
            ],
            vec![
                null.clone(),
                int(1_705_332_600),
                null.clone(),
                int(2),
                text(""),
                null.clone(),
                null.clone(),
                int(8),
            ],
            vec![
                int(-5),
                null.clone(),
                int(2),
                null.clone(),
                null,
                no,
                int(-1),
                int(9),
            ],
            // 1.005 rounds up, as its decimal does; the epoch on New
            // York's clock is five hours into it.
            vec![
                int(101),
                int(18_000),
                int(6),
                int(4),
                text("Ø"),
                yes,
                int(0),
                int(10),
            ],
        ];
        for compression in [
            Compression::UNCOMPRESSED,
            Compression::SNAPPY,
            Compression::ZSTD(ZstdLevel::default()),
        ] {
            trips(&path, compression);
            assert_eq!(rows(&path, &columns).unwrap(), expected, "{compression:?}");
        }
    }

    #[test]
    fn a_clock_stays_a_clock_and_an_integer_scales() {
        let scratch = Scratch::new();
        let path = scratch.0.join("trips.parquet");
        trips(&path, Compression::UNCOMPRESSED);
        let columns = [
            column("pickup_at", TIMESTAMP, "tpep_pickup_datetime", None),
            column(
                "payment",
                ColumnType::Numeric { scale: 3 },
                "payment_type",
                None,
            ),
            column("vendor", FARE, "VendorID", None),
        ];
        let int = Datum::Int;
        assert_eq!(
            rows(&path, &columns).unwrap(),
            [
                vec![int(1_705_314_600), int(1_000), int(100)],
                vec![int(1_705_314_600), int(2_000), Datum::Null],
                vec![Datum::Null, Datum::Null, int(200)],
                vec![int(0), int(4_000), int(600)],
            ]
        );
        assert_eq!(rows(&path, &[]).unwrap(), Vec::<Vec<Datum>>::new());
    }

    #[test]
    fn batches_hold_whole_rows_of_every_row_group() {
        let scratch = Scratch::new();
        let path = scratch.0.join("many.parquet");
        let schema = "message schema { OPTIONAL INT64 n; OPTIONAL DOUBLE half; }";
        let group = |from: i64, to: i64| {
            vec![
                Cells::Int64((from..to).map(|n| (n % 7 != 0).then_some(n)).collect()),
                Cells::Double((from..to).map(|n| Some(n as f64 / 2.0)).collect()),
            ]
        };
        let groups = vec![
            group(0, 20_000),
            group(20_000, 20_001),
            group(20_001, 30_000),
        ];
        write(&path, schema, Compression::SNAPPY, groups);
        let columns = [
            column("half", ColumnType::Numeric { scale: 1 }, "half", None),
            column("n", ColumnType::Int, "n", None),
        ];
        let columns: Vec<&Column> = columns.iter().collect();
        let (mut batches, mut next) = (Vec::new(), 0);
        let total = read(&path, &columns, |batch: &[Datum]| {
            batches.push(batch.len() / 2);
            for row in batch.chunks(2) {
                let n = if next % 7 == 0 {
                    Datum::Null
                } else {
                    Datum::Int(next)
                };
                assert_eq!(row, [Datum::Int(next * 5), n]);
                next += 1;
            }
            Ok::<(), String>(())
        })
        .unwrap();
        assert_eq!((total, next), (30_000, 30_000));
        assert_eq!(batches.iter().sum::<usize>(), 30_000);
        assert!(batches.iter().all(|rows| *rows <= BATCH));
        assert!(batches.len() >= 5, "{batches:?}");
    }

    #[test]
    fn the_callers_failure_stops_the_file() {
        let scratch = Scratch::new();
        let path = scratch.0.join("trips.parquet");
        trips(&path, Compression::UNCOMPRESSED);
        let id = column("id", ColumnType::Int, "id", None);
        let mut calls = 0;
        let stopped = read(&path, &[&id], |_: &[Datum]| {
            calls += 1;
            Err::<(), String>("the feed closed".into())
        });
        assert_eq!(stopped.unwrap_err(), "the feed closed");
        assert_eq!(calls, 1);
    }

    fn refused(path: &Path, columns: &[Column]) -> String {
        let error = rows(path, columns).unwrap_err();
        let (file, what) = error.split_once(": ").unwrap();
        assert_eq!(file, path.display().to_string());
        what.to_owned()
    }

    #[test]
    fn a_column_that_does_not_read_as_its_type_is_named() {
        let scratch = Scratch::new();
        let path = scratch.0.join("trips.parquet");
        trips(&path, Compression::UNCOMPRESSED);
        let pickup = "tpep_pickup_datetime";
        let cases = [
            (
                column("fare", FARE, "fare", None),
                "has no column fare, which column fare is read from",
            ),
            (
                column("vendor_id", ColumnType::Int, "vendorid", None),
                "has no column vendorid, which column vendor_id is read from",
            ),
            (
                column("fare", ColumnType::Int, "fare_amount", None),
                "column fare is read from fare_amount, which is DOUBLE in the file \
                 and doesn't read as an integer",
            ),
            (
                column("flag", FARE, "store_and_fwd_flag", None),
                "column flag is read from store_and_fwd_flag, which is BYTE_ARRAY in the file \
                 and doesn't read as a numeric of scale 2",
            ),
            (
                column("shared", ColumnType::Bool, "VendorID", None),
                "column shared is read from VendorID, which is INT32 in the file \
                 and doesn't read as a boolean",
            ),
            (
                column("flag", ColumnType::Text, "shared", None),
                "column flag is read from shared, which is BOOLEAN in the file \
                 and doesn't read as text",
            ),
            (
                column("at", TIMESTAMPTZ, "payment_type", None),
                "column at is read from payment_type, which is INT64 in the file \
                 and doesn't read as a timestamptz",
            ),
            (
                column("at", TIMESTAMP, "VendorID", None),
                "column at is read from VendorID, which is INT32 in the file \
                 and doesn't read as a timestamp",
            ),
            (
                column("at", TIMESTAMPTZ, pickup, None),
                "column at is read from tpep_pickup_datetime, which is a clock with no zone \
                 in the file; say whose with in ZONE",
            ),
            (
                column("at", TIMESTAMPTZ, pickup, Some("America/Gotham")),
                "column at is read from tpep_pickup_datetime, which is read in America/Gotham, \
                 which is no time zone",
            ),
            (
                column("at", TIMESTAMPTZ, "seen", Some("UTC")),
                "column at is read from seen, which is an instant in the file; drop the in ZONE",
            ),
            (
                column("at", TIMESTAMP, "seen", None),
                "column at is read from seen, which is an instant in the file; write timestamptz",
            ),
        ];
        for (column, why) in cases {
            assert_eq!(refused(&path, &[column]), why);
        }
    }

    #[test]
    fn a_value_that_is_no_value_of_its_type_is_named_by_its_row() {
        let scratch = Scratch::new();
        let path = scratch.0.join("bad.parquet");
        let schema = "message schema {
            OPTIONAL DOUBLE fare;
            OPTIONAL BYTE_ARRAY name;
            OPTIONAL INT64 big;
            OPTIONAL INT64 at (TIMESTAMP(MILLIS,false));
        }";
        let group = |fare: f64, name: &'static [u8], big: i64, at: i64| {
            vec![
                Cells::Double(vec![None, Some(fare)]),
                Cells::Bytes(vec![None, Some(name)]),
                Cells::Int64(vec![None, Some(big)]),
                Cells::Int64(vec![None, Some(at)]),
            ]
        };
        let groups = vec![
            group(1.0, b"ok", 1, 0),
            group(f64::NAN, b"\xff", i64::MAX, i64::MAX),
        ];
        write(&path, schema, Compression::UNCOMPRESSED, groups);
        let cases = [
            (
                column("fare", FARE, "fare", None),
                "row 4: can't read NaN as a numeric of scale 2 in column fare",
            ),
            (
                column("name", ColumnType::Text, "name", None),
                "row 4: can't read \"\u{fffd}\" as text in column name",
            ),
            (
                column("big", FARE, "big", None),
                "row 4: can't read 9223372036854775807 as a numeric of scale 2 in column big",
            ),
            (
                column("at", TIMESTAMPTZ, "at", Some("America/New_York")),
                "row 4: can't read 9223372036854775807 as a timestamptz in column at",
            ),
        ];
        for (column, why) in cases {
            assert_eq!(refused(&path, &[column]), why);
        }
    }

    #[test]
    fn timestamps_count_in_the_parts_their_column_says() {
        let scratch = Scratch::new();
        let path = scratch.0.join("stamps.parquet");
        let schema = "message schema {
            OPTIONAL INT64 nanos (TIMESTAMP(NANOS,true));
            OPTIONAL INT64 millis (TIMESTAMP(MILLIS,false));
            OPTIONAL INT64 old_millis (TIMESTAMP_MILLIS);
            OPTIONAL INT64 old_micros (TIMESTAMP_MICROS);
        }";
        let group = vec![
            Cells::Int64(vec![Some(90_000_000_000)]),
            Cells::Int64(vec![Some(90_999)]),
            Cells::Int64(vec![Some(90_000)]),
            Cells::Int64(vec![Some(90_000_000)]),
        ];
        write(&path, schema, Compression::UNCOMPRESSED, vec![group]);
        let columns = [
            column("nanos", TIMESTAMPTZ, "nanos", None),
            column("millis", TIMESTAMP, "millis", None),
            // A file from before timestamps said so holds instants.
            column("old_millis", TIMESTAMPTZ, "old_millis", None),
            column("old_micros", TIMESTAMPTZ, "old_micros", None),
        ];
        assert_eq!(rows(&path, &columns).unwrap(), [vec![Datum::Int(90); 4]]);
    }

    #[test]
    fn a_page_that_does_not_read_is_the_files_failure() {
        let scratch = Scratch::new();
        let path = scratch.0.join("torn.parquet");
        trips(&path, Compression::SNAPPY);
        // The first column's first page comes right after the file's
        // four bytes of magic; the footer stays whole.
        let mut bytes = fs::read(&path).unwrap();
        bytes[4..24].fill(0xff);
        fs::write(&path, bytes).unwrap();
        let why = refused(
            &path,
            &[column("vendor_id", ColumnType::Int, "VendorID", None)],
        );
        assert!(!why.starts_with("row "), "{why}");
        // The other columns still read.
        let id = column("id", ColumnType::Int, "id", None);
        assert_eq!(rows(&path, &[id]).unwrap().len(), 4);
    }

    #[test]
    fn a_file_that_is_no_parquet_is_named() {
        let scratch = Scratch::new();
        let id = column("id", ColumnType::Int, "id", None);
        let missing = scratch.0.join("missing.parquet");
        assert!(refused(&missing, std::slice::from_ref(&id)).starts_with("No such file"));
        let text = scratch.0.join("trips.csv");
        fs::write(&text, "id\n1\n").unwrap();
        let why = refused(&text, std::slice::from_ref(&id));
        assert!(why.contains("Parquet file too small"), "{why}");

        // A file cut short after its first pages: the footer is gone.
        let cut = scratch.0.join("cut.parquet");
        trips(&cut, Compression::UNCOMPRESSED);
        let whole = fs::read(&cut).unwrap();
        fs::write(&cut, &whole[..whole.len() / 2]).unwrap();
        let why = refused(&cut, &[id]);
        assert!(why.contains("Parquet"), "{why}");
    }

    #[test]
    fn a_column_inside_a_list_is_no_column_of_a_table() {
        let scratch = Scratch::new();
        let path = scratch.0.join("nested.parquet");
        let schema = "message schema { REPEATED INT64 tags; }";
        let schema = Arc::new(parse_message_type(schema).unwrap());
        let file = File::create(&path).unwrap();
        let mut writer = SerializedFileWriter::new(file, schema, Default::default()).unwrap();
        let mut rows = writer.next_row_group().unwrap();
        let mut tags = rows.next_column().unwrap().unwrap();
        let typed = tags.typed::<Int64Type>();
        typed
            .write_batch(&[1, 2], Some(&[1, 1]), Some(&[0, 1]))
            .unwrap();
        tags.close().unwrap();
        rows.close().unwrap();
        writer.close().unwrap();
        assert_eq!(
            refused(&path, &[column("tags", ColumnType::Int, "tags", None)]),
            "has no column tags, which column tags is read from"
        );
    }
}
