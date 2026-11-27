# Usage metering

How do you bill each account for its usage? A billing service, for
example Stripe Meters, Lago or OpenMeter, charges for the numbers that
you send to it. Your app writes a row for each use. Straum keeps the
sums of these rows, and an ask gives the number for each account.

## The data

The examples use the usage events from the Azure Functions trace of
2019. Each event is one call of a function. `account_id` is the owner
of the function, `kind` is its trigger and `amount` is the duration of
the call in milliseconds. The events start on 2026-11-02 and cover 14
days.

The trace has about 900 million calls each day, so the script keeps
one call in 1000. Each sum on this page is thus near 1/1000 of the
usage in the trace. The docstring of `bench/load_usage_events.py` tells
which columns come from the trace.

Run this command in the repository to make the events and open Straum
on them:

```sh
just usage-events
```

The recipe loads the events into Docker with the `usage` view of
`bench/usage.sql`, then opens `straum shell`. It prints the port that
Straum answers on. Add `--days 1` to make only the first day. The
recipe gives each other flag to `just try`, as `--image`.

## The bill for a month

An app shows each account its usage in this month. Ask for the sum of
one account from the first day of the month to the first day of the
next month:

```sh
curl --json '{
  "query": "SELECT account_id, events, amount FROM usage WHERE account_id = $1 AND bucket >= $2 AND bucket < $3 GROUP BY account_id",
  "parameters": ["49d849c7-58f4-ae2d-7ce7-b13ad4e65694", "2026-11-01T00:00:00Z", "2026-12-01T00:00:00Z"]
}' http://localhost:8080/ask
```

```json
{
  "as_of": "2026-11-25T13:22:29.228656419Z",
  "rows": [
    {"account_id": "49d849c7-58f4-ae2d-7ce7-b13ad4e65694", "events": 1800971, "amount": 2793288034.66}
  ]
}
```

Straum answers from its buckets. It does not read the events again.
Your backend puts the account of the user that signed in into the ask
([Who can ask](../deploy.md#who-can-ask)).

To show the usage of each day, group by the day buckets of the view
([Rows a day or a week apart](../asks.md#rows-a-day-or-a-week-apart)):

```sh
curl --json '{
  "query": "SELECT buckets(bucket, '\''1 day'\''), events, amount FROM usage WHERE account_id = $1 AND bucket >= $2 AND bucket < $3 GROUP BY buckets(bucket, '\''1 day'\'')",
  "parameters": ["49d849c7-58f4-ae2d-7ce7-b13ad4e65694", "2026-11-09T00:00:00Z", "2026-11-12T00:00:00Z"]
}' http://localhost:8080/ask
```

```json
{
  "as_of": "2026-11-25T13:22:29.228656169Z",
  "rows": [
    {"bucket": "2026-11-09T00:00:00Z", "events": 127853, "amount": 188104630.14},
    {"bucket": "2026-11-10T00:00:00Z", "events": 118914, "amount": 196868079.65},
    {"bucket": "2026-11-11T00:00:00Z", "events": 125241, "amount": 198666787.54}
  ]
}
```

## The usage of the last hour

A billing service adds the numbers that you send to it. Each hour, ask
for the usage of all accounts in the hour before, and send each row:

```sh
curl --json '{
  "query": "SELECT account_id, events, amount FROM usage WHERE bucket >= $1 AND bucket < $2 GROUP BY account_id",
  "parameters": ["2026-11-15T13:00:00Z", "2026-11-15T14:00:00Z"]
}' http://localhost:8080/ask
```

```json
{
  "as_of": "2026-11-25T13:23:00.302668712Z",
  "rows": [
    {"account_id": "0008160d-e0dd-2ee1-84e8-562e158d6202", "events": 1, "amount": 704.56},
    {"account_id": "00120a3b-2acd-94c5-dab1-7ee482c500a7", "events": 7, "amount": 1.69},
    {"account_id": "00ee0bc3-54e7-0de1-6189-8935400933fd", "events": 4, "amount": 52.89}
  ]
}
```

The answer has a row for each account with usage in that hour: 1072
rows here. Only the first three are shown. An account whose events
have no `amount` has `null` there. Send its `events` alone.

Give each row that you send a key made of the account and the hour. If
the billing service drops a second row with the same key, a send that
runs again does not charge twice.

The answer holds each commit made before `as_of`. Your app can write an
event later, with an `at` in an hour that you sent. Your sum for that
hour does not have it. Wait some minutes after the end of the hour
before you ask.

## The same numbers in SQL

PostgreSQL gives the same numbers for the bill of the month:

```sql
SELECT count(*), sum(amount)
FROM public.events
WHERE account_id = '49d849c7-58f4-ae2d-7ce7-b13ad4e65694'
  AND at >= '2026-11-01T00:00:00Z' AND at < '2026-12-01T00:00:00Z';
```

```text
  count  |      sum
---------+---------------
 1800971 | 2793288034.66
(1 row)
```

`just usage-events` prints the `psql` command for the PostgreSQL that
holds the events. [Try it on your data](../your-data.md) compares an
ask with your own SQL.

Next: [Asks and endpoints](../asks.md) gives each part of an ask.
