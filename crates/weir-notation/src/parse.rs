use std::fmt;

use weir_domain::{
    Agg, Cond, Dim, Fact, Grain, Ident, KeyPath, Literal, Measure, Ref, Table, Time,
};

use crate::lex::{Tok, lex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    /// 1-based.
    pub line: usize,
    pub msg: String,
}

pub(crate) fn parse(src: &str) -> Result<(Vec<Dim>, Vec<Fact>), SyntaxError> {
    let mut dims = Vec::new();
    let mut facts = Vec::new();
    let mut open: Option<Block> = None;
    for (i, text) in src.lines().enumerate() {
        let line = i + 1;
        let err = |msg: String| SyntaxError { line, msg };
        let toks = lex(text).map_err(err)?;
        if toks.is_empty() {
            continue;
        }
        let mut cur = Cursor {
            toks: &toks,
            pos: 0,
        };
        if text.starts_with([' ', '\t']) {
            let Some(block) = open.as_mut() else {
                return Err(err("indented line outside a dim or fact block".into()));
            };
            block.body(&mut cur).map_err(err)?;
        } else {
            if let Some(block) = open.take() {
                block.close(&mut dims, &mut facts)?;
            }
            open = Some(Block::open(&mut cur, line).map_err(err)?);
        }
        cur.end().map_err(err)?;
    }
    if let Some(block) = open {
        block.close(&mut dims, &mut facts)?;
    }
    Ok((dims, facts))
}

enum Block {
    Dim(Dim),
    Fact(FactDraft),
}

/// A fact whose `time` line may not have been seen yet.
struct FactDraft {
    line: usize,
    name: Ident,
    table: Table,
    key: Ident,
    time: Option<Time>,
    refs: Vec<Ref>,
    by: Vec<Ident>,
    append_only: bool,
    measures: Vec<Measure>,
}

impl Block {
    /// `dim|fact NAME = SCHEMA.TABLE key COLUMN`
    fn open(cur: &mut Cursor, line: usize) -> Result<Block, String> {
        let is_fact = match cur.word("dim or fact")?.as_str() {
            "dim" => false,
            "fact" => true,
            other => return Err(format!("expected dim or fact, found {other}")),
        };
        let name = cur.ident("name")?;
        cur.sym("=")?;
        let schema = cur.ident("schema")?;
        cur.sym(".")?;
        let table = Table {
            schema,
            name: cur.ident("table")?,
        };
        cur.keyword("key")?;
        let key = cur.ident("key column")?;
        Ok(if is_fact {
            Block::Fact(FactDraft {
                line,
                name,
                table,
                key,
                time: None,
                refs: Vec::new(),
                by: Vec::new(),
                append_only: false,
                measures: Vec::new(),
            })
        } else {
            Block::Dim(Dim {
                name,
                table,
                key,
                attrs: Vec::new(),
            })
        })
    }

    fn body(&mut self, cur: &mut Cursor) -> Result<(), String> {
        match self {
            Block::Dim(dim) => {
                cur.keyword("attrs")?;
                cur.idents(&mut dim.attrs)
            }
            Block::Fact(fact) => fact.body(cur),
        }
    }

    fn close(self, dims: &mut Vec<Dim>, facts: &mut Vec<Fact>) -> Result<(), SyntaxError> {
        match self {
            Block::Dim(dim) => dims.push(dim),
            Block::Fact(f) => {
                let Some(time) = f.time else {
                    return Err(SyntaxError {
                        line: f.line,
                        msg: format!("fact {} has no time line", f.name),
                    });
                };
                facts.push(Fact {
                    name: f.name,
                    table: f.table,
                    key: f.key,
                    time,
                    refs: f.refs,
                    by: f.by,
                    append_only: f.append_only,
                    measures: f.measures,
                });
            }
        }
        Ok(())
    }
}

impl FactDraft {
    fn body(&mut self, cur: &mut Cursor) -> Result<(), String> {
        match cur.word("time, ref, by, append or a measure")?.as_str() {
            // time COLUMN / GRAIN
            "time" => {
                if self.time.is_some() {
                    return Err("time declared twice".into());
                }
                let column = cur.ident("time column")?;
                cur.sym("/")?;
                self.time = Some(Time {
                    column,
                    grain: cur.grain()?,
                });
            }
            // ref NAME -> DIM via COLUMN
            "ref" => {
                let name = cur.ident("ref name")?;
                cur.sym("->")?;
                let dim = cur.ident("dim")?;
                cur.keyword("via")?;
                self.refs.push(Ref {
                    name,
                    dim,
                    via: cur.ident("column")?,
                });
            }
            "by" => cur.idents(&mut self.by)?,
            "append" => self.append_only = true,
            // NAME AGG [COLUMN] [where COND {and COND}]
            name => {
                let name = Ident::new(name)
                    .ok_or_else(|| format!("measure name {name} is not a lowercase identifier"))?;
                let agg = cur.agg()?;
                let mut filter = Vec::new();
                if cur.eat_keyword("where") {
                    filter.push(cur.cond()?);
                    while cur.eat_keyword("and") {
                        filter.push(cur.cond()?);
                    }
                }
                self.measures.push(Measure { name, agg, filter });
            }
        }
        Ok(())
    }
}

struct Cursor<'t> {
    toks: &'t [Tok],
    pos: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn found(&self) -> String {
        self.peek()
            .map_or_else(|| "end of line".into(), Tok::to_string)
    }

    fn end(&self) -> Result<(), String> {
        match self.peek() {
            None => Ok(()),
            Some(t) => Err(format!("unexpected {t}")),
        }
    }

    fn word(&mut self, what: &str) -> Result<String, String> {
        match self.peek() {
            Some(Tok::Word(w)) => {
                let w = w.clone();
                self.pos += 1;
                Ok(w)
            }
            _ => Err(format!("expected {what}, found {}", self.found())),
        }
    }

    fn ident(&mut self, what: &str) -> Result<Ident, String> {
        let w = self.word(what)?;
        Ident::new(&w).ok_or_else(|| format!("{what} {w} is not a lowercase identifier"))
    }

    fn idents(&mut self, into: &mut Vec<Ident>) -> Result<(), String> {
        into.push(self.ident("column")?);
        while self.eat_sym(",") {
            into.push(self.ident("column")?);
        }
        Ok(())
    }

    fn eat_sym(&mut self, s: &str) -> bool {
        let hit = matches!(self.peek(), Some(Tok::Sym(t)) if *t == s);
        self.pos += usize::from(hit);
        hit
    }

    fn eat_keyword(&mut self, k: &str) -> bool {
        let hit = matches!(self.peek(), Some(Tok::Word(w)) if w == k);
        self.pos += usize::from(hit);
        hit
    }

    fn sym(&mut self, s: &str) -> Result<(), String> {
        if self.eat_sym(s) {
            Ok(())
        } else {
            Err(format!("expected {s}, found {}", self.found()))
        }
    }

    fn keyword(&mut self, k: &str) -> Result<(), String> {
        if self.eat_keyword(k) {
            Ok(())
        } else {
            Err(format!("expected {k}, found {}", self.found()))
        }
    }

    /// A count and a unit, e.g. `15m`.
    fn grain(&mut self) -> Result<Grain, String> {
        let w = self.word("grain such as 15m")?;
        let (count, unit) = w.split_at(w.len() - 1);
        let scale = match unit {
            "s" => 1,
            "m" => 60,
            "h" => 3_600,
            "d" => 86_400,
            _ => return Err(format!("grain {w} needs a unit: s, m, h or d")),
        };
        let count: u32 = count.parse().map_err(|_| format!("bad grain {w}"))?;
        count
            .checked_mul(scale)
            .map(Grain)
            .ok_or_else(|| format!("grain {w} too large"))
    }

    fn agg(&mut self) -> Result<Agg, String> {
        const AGGS: &str = "count, sum, min, max, avg or count_distinct";
        let agg: fn(Ident) -> Agg = match self.word(AGGS)?.as_str() {
            "count" => return Ok(Agg::Count),
            "sum" => Agg::Sum,
            "min" => Agg::Min,
            "max" => Agg::Max,
            "avg" => Agg::Avg,
            "count_distinct" => Agg::CountDistinct,
            other => return Err(format!("expected {AGGS}, found {other}")),
        };
        Ok(agg(self.ident("column")?))
    }

    /// `PATH = LITERAL` or `PATH in (LITERAL, ...)`
    fn cond(&mut self) -> Result<Cond, String> {
        let head = self.ident("column or ref")?;
        let attr = if self.eat_sym(".") {
            Some(self.ident("attribute")?)
        } else {
            None
        };
        let path = KeyPath { head, attr };
        let mut values = Vec::new();
        if self.eat_sym("=") {
            values.push(self.literal()?);
        } else if self.eat_keyword("in") {
            self.sym("(")?;
            values.push(self.literal()?);
            while self.eat_sym(",") {
                values.push(self.literal()?);
            }
            self.sym(")")?;
        } else {
            return Err(format!("expected = or in, found {}", self.found()));
        }
        Ok(Cond { path, values })
    }

    fn literal(&mut self) -> Result<Literal, String> {
        let lit = match self.peek() {
            Some(Tok::Int(n)) => Literal::Int(*n),
            Some(Tok::Text(s)) => Literal::Text(s.clone()),
            Some(Tok::Word(w)) if w == "true" => Literal::Bool(true),
            Some(Tok::Word(w)) if w == "false" => Literal::Bool(false),
            _ => {
                return Err(format!(
                    "expected a number, 'text', true or false, found {}",
                    self.found()
                ));
            }
        };
        self.pos += 1;
        Ok(lit)
    }
}

impl fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.msg)
    }
}

impl std::error::Error for SyntaxError {}
