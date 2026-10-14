use crate::db::{Envis, col, from_sql_time, sql_err, to_sql_time};
use crate::pack::{
    Calendar, IndexEntry, Unit, covered, decode_index, encode_index, is_break, row_time,
    sparse_step, time_index,
};
use crate::{Error, Result};

const COLS: &str =
    "keyTime, endTime, ArchDef, CAST(Period AS bigint), CAST(Count AS bigint), TimeIndexes";

const WRITE_CHUNK: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub inserted: usize,
    pub duplicates: usize,
    pub refused: usize,
}

#[derive(Clone, Debug)]
struct PackRow {
    key_time: u64,
    end_time: u64,
    def: Option<i32>,
    period: u64,
    count: u32,
    index: Vec<IndexEntry>,
}

struct Open {
    stored_key: u64,
    from: u64,
    to: u64,
    count: u32,
    end: Option<u64>,
    index: Vec<IndexEntry>,
    pending: Vec<u8>,
    last_rows: Vec<Vec<u8>>,
    older: Vec<(u64, Vec<u8>)>,
    refusing: bool,
}

pub struct Appender<'a> {
    db: &'a mut Envis,
    calendar: &'a Calendar,
    meas: i32,
    archive: u8,
    def: i32,
    period: u64,
    row_len: usize,
    unit: Unit,
    open: Option<Open>,
    report: Report,
}

impl<'a> Appender<'a> {
    pub async fn begin(
        db: &'a mut Envis,
        calendar: &'a Calendar,
        meas: i32,
        archive: u8,
        def: i32,
        period: u64,
        row_len: usize,
    ) -> Result<Self> {
        db.exec("SET XACT_ABORT ON; BEGIN TRANSACTION").await?;
        db.client
            .query(
                "SELECT TOP 1 keyTime FROM dbo.UniArchiveBinPack WITH (XLOCK, ROWLOCK, HOLDLOCK)
                 WHERE keymeasName = @P1 AND keyArchID = @P2",
                &[&meas, &archive],
            )
            .await
            .map_err(sql_err)?
            .into_results()
            .await
            .map_err(sql_err)?;
        Ok(Self {
            db,
            calendar,
            meas,
            archive,
            def,
            period,
            row_len,
            unit: Unit::for_archive(period, row_len),
            open: None,
            report: Report::default(),
        })
    }

    pub async fn add(&mut self, row: &[u8]) -> Result<()> {
        if row.len() != self.row_len {
            return Err(Error::RowLength {
                expected: self.row_len,
                got: row.len(),
            });
        }
        let t = row_time(row).ok_or(Error::RowLength {
            expected: self.row_len,
            got: row.len(),
        })?;
        if self.open.as_ref().is_none_or(|o| t <= o.from || t > o.to) {
            self.close().await?;
            self.open = Some(self.target(t).await?);
        }
        let Some(open) = self.open.as_mut() else {
            return Ok(());
        };
        if open.refusing {
            self.report.refused = self.report.refused.saturating_add(1);
            return Ok(());
        }
        let continuous = self.period > 0;
        match open.end {
            Some(end) if t <= end => {
                if continuous {
                    if covered(&open.index, end)
                        .iter()
                        .any(|&(a, b)| a <= t && t <= b)
                    {
                        self.report.duplicates = self.report.duplicates.saturating_add(1);
                        return Ok(());
                    }
                } else if t == end {
                    if open.last_rows.iter().any(|r| r.as_slice() == row) {
                        self.report.duplicates = self.report.duplicates.saturating_add(1);
                        return Ok(());
                    }
                    open.last_rows.push(row.to_vec());
                    push_row(open, t, row, self.period, self.row_len);
                    self.report.inserted = self.report.inserted.saturating_add(1);
                    return Ok(());
                }
                open.older.push((t, row.to_vec()));
            }
            _ => {
                if !continuous {
                    open.last_rows = vec![row.to_vec()];
                }
                push_row(open, t, row, self.period, self.row_len);
                self.report.inserted = self.report.inserted.saturating_add(1);
            }
        }
        if open.pending.len() >= WRITE_CHUNK {
            self.flush().await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        let Some(open) = self.open.as_mut() else {
            return Ok(());
        };
        if open.pending.is_empty() {
            return Ok(());
        }
        let bytes = std::mem::take(&mut open.pending);
        let key = to_sql_time(open.stored_key);
        let n = self
            .db
            .client
            .execute(
                "UPDATE dbo.UniArchiveBinPack SET DataBuf.WRITE(@P4, NULL, 0)
                 WHERE keymeasName = @P1 AND keyArchID = @P2 AND keyTime = @P3",
                &[&self.meas, &self.archive, &key, &bytes.as_slice()],
            )
            .await
            .map_err(sql_err)?
            .total();
        one_row(n)
    }

    async fn close(&mut self) -> Result<()> {
        self.flush().await?;
        let Some(mut open) = self.open.take() else {
            return Ok(());
        };
        if open.refusing {
            return Ok(());
        }
        if !open.older.is_empty() {
            self.merge(&mut open).await?;
        }
        let Some(end) = open.end else {
            self.db
                .client
                .execute(
                    "DELETE FROM dbo.UniArchiveBinPack
                     WHERE keymeasName = @P1 AND keyArchID = @P2 AND keyTime = @P3 AND Count = 0",
                    &[&self.meas, &self.archive, &to_sql_time(open.stored_key)],
                )
                .await
                .map_err(sql_err)?;
            return Ok(());
        };
        if self.period > 0 && open.count > 0 {
            open.index.push(IndexEntry {
                row: open.count.saturating_sub(1),
                time: end,
            });
        }
        let first = open.index.first().map_or(open.stored_key, |e| e.time);
        let index = encode_index(&open.index);
        let count = i64::from(open.count);
        let n = self
            .db
            .client
            .execute(
                "UPDATE dbo.UniArchiveBinPack SET keyTime = @P4, TimeIndexes = @P5, Count = @P6,
                     endTime = @P7
                 WHERE keymeasName = @P1 AND keyArchID = @P2 AND keyTime = @P3",
                &[
                    &self.meas,
                    &self.archive,
                    &to_sql_time(open.stored_key),
                    &to_sql_time(first),
                    &index.as_deref(),
                    &count,
                    &to_sql_time(end),
                ],
            )
            .await
            .map_err(sql_err)?
            .total();
        one_row(n)
    }

    async fn merge(&mut self, open: &mut Open) -> Result<()> {
        let mut older = std::mem::take(&mut open.older);
        older.sort_by_key(|(t, _)| *t);
        let first_new = older.first().map_or(0, |(t, _)| *t);
        let continuous = self.period > 0;
        let start = open
            .index
            .iter()
            .enumerate()
            .filter(|(i, e)| {
                if continuous {
                    e.time <= first_new && i % 2 == 0
                } else {
                    e.time < first_new
                }
            })
            .map(|(_, e)| e.row)
            .next_back()
            .unwrap_or(0);
        let len = u64::try_from(self.row_len).unwrap_or(u64::MAX);
        let offset = u64::from(start).saturating_mul(len);
        let offset_sql = i64::try_from(offset).unwrap_or(i64::MAX);
        let tail = self.read_from(open.stored_key, offset_sql).await?;
        let mut rows: Vec<(u64, Vec<u8>)> = tail
            .chunks_exact(self.row_len)
            .filter_map(|r| Some((row_time(r)?, r.to_vec())))
            .collect();
        for (t, row) in older {
            let dup = rows
                .iter()
                .any(|(rt, r)| *rt == t && (continuous || r.as_slice() == row.as_slice()));
            if dup {
                self.report.duplicates = self.report.duplicates.saturating_add(1);
                continue;
            }
            let at = rows.partition_point(|(rt, _)| *rt <= t);
            rows.insert(at, (t, row));
            self.report.inserted = self.report.inserted.saturating_add(1);
        }
        let bytes: Vec<u8> = rows.iter().flat_map(|(_, r)| r.iter().copied()).collect();
        let key = to_sql_time(open.stored_key);
        let n = self
            .db
            .client
            .execute(
                "UPDATE dbo.UniArchiveBinPack SET DataBuf.WRITE(@P4, @P5, NULL)
                 WHERE keymeasName = @P1 AND keyArchID = @P2 AND keyTime = @P3",
                &[
                    &self.meas,
                    &self.archive,
                    &key,
                    &bytes.as_slice(),
                    &offset_sql,
                ],
            )
            .await
            .map_err(sql_err)?
            .total();
        one_row(n)?;
        let times: Vec<u64> = rows.iter().map(|(t, _)| *t).collect();
        open.index.retain(|e| e.row < start);
        let mut tail_index = time_index(&times, self.period, self.row_len);
        if continuous {
            tail_index.pop();
        } else {
            let step = sparse_step(self.row_len);
            tail_index = times
                .iter()
                .enumerate()
                .filter_map(|(i, &t)| {
                    let row = start.saturating_add(u32::try_from(i).ok()?);
                    (row.checked_rem(step) == Some(0)).then_some(IndexEntry { row, time: t })
                })
                .collect();
        }
        for e in &mut tail_index {
            if continuous {
                e.row = e.row.saturating_add(start);
            }
        }
        open.index.extend(tail_index);
        open.count = start.saturating_add(u32::try_from(rows.len()).unwrap_or(u32::MAX));
        open.end = rows.last().map(|(t, _)| *t).or(open.end);
        if !continuous {
            let end = open.end;
            open.last_rows = rows
                .iter()
                .rev()
                .take_while(|(t, _)| Some(*t) == end)
                .map(|(_, r)| r.clone())
                .collect();
        }
        Ok(())
    }

    async fn read_from(&mut self, key: u64, offset: i64) -> Result<Vec<u8>> {
        let row = self
            .db
            .client
            .query(
                "SELECT SUBSTRING(DataBuf, @P4 + 1, DATALENGTH(DataBuf)) FROM dbo.UniArchiveBinPack
                 WHERE keymeasName = @P1 AND keyArchID = @P2 AND keyTime = @P3",
                &[&self.meas, &self.archive, &to_sql_time(key), &offset],
            )
            .await
            .map_err(sql_err)?
            .into_row()
            .await
            .map_err(sql_err)?;
        Ok(match row {
            Some(r) => col::<&[u8]>(&r, 0)?.map(<[u8]>::to_vec).unwrap_or_default(),
            None => Vec::new(),
        })
    }

    async fn pack(&mut self, sql: &str, t: u64, other: u64) -> Result<Option<PackRow>> {
        let row = self
            .db
            .client
            .query(
                sql,
                &[
                    &self.meas,
                    &self.archive,
                    &to_sql_time(t),
                    &to_sql_time(other),
                ],
            )
            .await
            .map_err(sql_err)?
            .into_row()
            .await
            .map_err(sql_err)?;
        let Some(r) = row else {
            return Ok(None);
        };
        Ok(Some(PackRow {
            key_time: col::<time::PrimitiveDateTime>(&r, 0)?.map_or(0, from_sql_time),
            end_time: col::<time::PrimitiveDateTime>(&r, 1)?.map_or(0, from_sql_time),
            def: col::<i32>(&r, 2)?,
            period: col::<i64>(&r, 3)?
                .and_then(|p| u64::try_from(p).ok())
                .unwrap_or(0),
            count: col::<i64>(&r, 4)?
                .and_then(|c| u32::try_from(c).ok())
                .unwrap_or(0),
            index: col::<&[u8]>(&r, 5)?.map(decode_index).unwrap_or_default(),
        }))
    }

    async fn same(&mut self, p: &PackRow) -> Result<bool> {
        if p.period != self.period {
            return Ok(false);
        }
        let Some(def) = p.def else {
            return Ok(false);
        };
        if def == self.def {
            return Ok(true);
        }
        let (Some(a), Some(b)) = (
            self.db.stored_definition(def).await?,
            self.db.stored_definition(self.def).await?,
        ) else {
            return Ok(false);
        };
        Ok(a.fields.same_layout(&b.fields))
    }

    async fn next_pack(&mut self, after: u64, to: u64) -> Result<Option<PackRow>> {
        self.pack(
            &format!(
                "SELECT TOP 1 {COLS} FROM dbo.UniArchiveBinPack
                 WHERE keymeasName = @P1 AND keyArchID = @P2 AND keyTime > @P3 AND keyTime <= @P4
                 ORDER BY keyTime"
            ),
            after,
            to,
        )
        .await
    }

    async fn cap(&mut self, after: u64, to: u64) -> Result<u64> {
        Ok(self
            .next_pack(after, to)
            .await?
            .map_or(to, |n| to.min(n.key_time.saturating_sub(1))))
    }

    async fn target(&mut self, t: u64) -> Result<Open> {
        let (mut from, interval_end) = self
            .calendar
            .interval(self.unit, t)
            .map_err(|e| Error::Time(e.to_string()))?;
        let mut to = interval_end;
        let newest = self
            .pack(
                &format!(
                    "SELECT TOP 1 {COLS} FROM dbo.UniArchiveBinPack
                     WHERE keymeasName = @P1 AND keyArchID = @P2 AND keyTime <= @P3 AND endTime > @P4
                     ORDER BY keyTime DESC"
                ),
                t,
                from,
            )
            .await?;
        let prev = if let Some(p) = newest {
            if t <= p.end_time {
                if !self.same(&p).await? {
                    return Ok(refusing(p.key_time.saturating_sub(1), p.end_time));
                }
                let from = from.min(p.key_time.saturating_sub(1));
                let to = self.cap(t, to).await?;
                return self.reopen(p, from, to).await;
            }
            Some(p)
        } else {
            None
        };
        let next = self.next_pack(t, to).await?;
        if let Some(n) = &next {
            to = to.min(n.key_time.saturating_sub(1));
        }
        if let Some(p) = prev {
            if p.key_time > from && self.extends(&p) {
                return self.reopen(p, from, to).await;
            }
            from = from.max(p.end_time);
        }
        if let Some(p) = next
            && p.end_time <= interval_end
            && self.extends(&p)
        {
            let to = self.cap(p.key_time, interval_end).await?;
            return self.reopen(p, from, to).await;
        }
        self.create(t, from, to).await
    }

    fn extends(&self, p: &PackRow) -> bool {
        p.def == Some(self.def) && p.period == self.period
    }

    async fn reopen(&mut self, p: PackRow, from: u64, to: u64) -> Result<Open> {
        let last_rows = if self.period == 0 && p.count > 0 {
            self.rows_at_end(p.key_time, p.count, p.end_time).await?
        } else {
            Vec::new()
        };
        let mut index = p.index;
        if self.period > 0 {
            index.pop();
        }
        Ok(Open {
            stored_key: p.key_time,
            from,
            to: to.max(p.end_time),
            count: p.count,
            end: (p.count > 0).then_some(p.end_time),
            index,
            pending: Vec::new(),
            last_rows,
            older: Vec::new(),
            refusing: false,
        })
    }

    async fn rows_at_end(&mut self, key: u64, count: u32, end: u64) -> Result<Vec<Vec<u8>>> {
        let len = u64::try_from(self.row_len).unwrap_or(u64::MAX);
        let mut take = 16_u32;
        loop {
            let first = count.saturating_sub(take);
            let offset = i64::try_from(u64::from(first).saturating_mul(len)).unwrap_or(i64::MAX);
            let tail = self.read_from(key, offset).await?;
            let rows: Vec<Vec<u8>> = tail
                .chunks_exact(self.row_len)
                .rev()
                .take_while(|r| row_time(r) == Some(end))
                .map(<[u8]>::to_vec)
                .collect();
            let all = u32::try_from(rows.len()).unwrap_or(u32::MAX);
            if all < count.saturating_sub(first) || first == 0 {
                return Ok(rows);
            }
            take = take.saturating_mul(4);
        }
    }

    async fn create(&mut self, t: u64, from: u64, to: u64) -> Result<Open> {
        let period = i64::try_from(self.period).unwrap_or(i64::MAX);
        let key = to_sql_time(t);
        self.db
            .client
            .execute(
                "INSERT INTO dbo.UniArchiveBinPack (keymeasName, keyArchID, keyTime, ArchDef, Period,
                     Count, endTime, TimeIndexes, StreamId, DataBuf)
                 VALUES (@P1, @P2, @P3, @P4, @P5, 0, @P3, NULL, NEWID(), 0x)",
                &[&self.meas, &self.archive, &key, &self.def, &period],
            )
            .await
            .map_err(sql_err)?;
        Ok(Open {
            stored_key: t,
            from,
            to,
            count: 0,
            end: None,
            index: Vec::new(),
            pending: Vec::new(),
            last_rows: Vec::new(),
            older: Vec::new(),
            refusing: false,
        })
    }

    pub async fn commit(mut self) -> Result<Report> {
        match self.close().await {
            Ok(()) => {
                self.db.exec("COMMIT TRANSACTION").await?;
                Ok(self.report)
            }
            Err(e) => {
                let _ = self
                    .db
                    .exec("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION")
                    .await;
                Err(e)
            }
        }
    }

    pub async fn rollback(self) -> Result<()> {
        self.db
            .exec("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION")
            .await
    }
}

fn push_row(open: &mut Open, t: u64, row: &[u8], period: u64, row_len: usize) {
    if period > 0 {
        match open.end {
            None if open.index.is_empty() => open.index.push(IndexEntry { row: 0, time: t }),
            Some(end) if is_break(end, t, period) => {
                open.index.push(IndexEntry {
                    row: open.count.saturating_sub(1),
                    time: end,
                });
                open.index.push(IndexEntry {
                    row: open.count,
                    time: t,
                });
            }
            _ => {}
        }
    } else if open.count.checked_rem(sparse_step(row_len)) == Some(0) {
        open.index.push(IndexEntry {
            row: open.count,
            time: t,
        });
    }
    open.pending.extend_from_slice(row);
    open.count = open.count.saturating_add(1);
    open.end = Some(t);
}

fn one_row(n: u64) -> Result<()> {
    if n == 1 {
        Ok(())
    } else {
        Err(Error::Sql(format!("pack update hit {n} rows")))
    }
}

const fn refusing(from: u64, to: u64) -> Open {
    Open {
        stored_key: 0,
        from,
        to,
        count: 0,
        end: None,
        index: Vec::new(),
        pending: Vec::new(),
        last_rows: Vec::new(),
        older: Vec::new(),
        refusing: true,
    }
}
