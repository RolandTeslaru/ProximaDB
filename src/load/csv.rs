//! CSV datasets: a header row naming the columns, then text values.

use std::collections::HashMap;
use std::path::Path;

use crate::core::Kind;
use crate::storage::property::Cell;

use rayon::prelude::*;

use super::source::{BATCH_ROWS, Batch, Find, Key, LoadError, cell_from_text};


struct CsvBatch<'h> {
    records: Vec<csv::StringRecord>,
    index: &'h HashMap<String, usize>,
    path: &'h Path,
}


impl CsvBatch<'_> {
    fn column(&self, name: &str) -> Result<usize, LoadError> {
        self.index
            .get(name)
            .copied()
            .ok_or_else(|| LoadError::at(self.path, format!("no column {name}")))
    }
}


impl Batch for CsvBatch<'_> {
    fn rows(&self) -> usize {
        self.records.len()
    }


    fn each_key(
        &self,
        column: &str,
        each: &mut dyn FnMut(usize, Option<&[u8]>),
    ) -> Result<(), LoadError> {
        let at = self.column(column)?;

        for (row, record) in self.records.iter().enumerate() {
            let value = record.get(at).unwrap_or("");

            each(row, (!value.is_empty()).then_some(value.as_bytes()));
        }

        Ok(())
    }


    fn lookup_keys(&self, column: &str, find: &Find) -> Result<Vec<Key>, LoadError> {
        let at = self.column(column)?;

        Ok(self
            .records
            .par_iter()
            .map(|record| {
                let value = record.get(at).unwrap_or("");

                Key::of((!value.is_empty()).then_some(value.as_bytes()), find)
            })
            .collect())
    }


    fn cells(&self, column: &str, kind: Kind) -> Result<Vec<Option<Cell>>, LoadError> {
        let at = self.column(column)?;

        Ok(self
            .records
            .iter()
            .map(|record| cell_from_text(record.get(at).unwrap_or(""), kind))
            .collect())
    }
}


pub(crate) fn read(
    path: &Path,
    columns: &[&str],
    each: &mut dyn FnMut(&dyn Batch) -> Result<(), LoadError>,
) -> Result<(), LoadError> {
    let mut reader = csv::Reader::from_path(path).map_err(|e| LoadError::at(path, e))?;

    let headers = reader.headers().map_err(|e| LoadError::at(path, e))?;
    let index: HashMap<String, usize> = headers
        .iter()
        .enumerate()
        .map(|(at, name)| (name.to_string(), at))
        .collect();

    for column in columns {
        if !index.contains_key(*column) {
            return Err(LoadError::at(path, format!("no column {column}")));
        }
    }

    let mut records = Vec::with_capacity(BATCH_ROWS);

    for record in reader.records() {
        records.push(record.map_err(|e| LoadError::at(path, e))?);

        if records.len() == BATCH_ROWS {
            let full = std::mem::replace(&mut records, Vec::with_capacity(BATCH_ROWS));

            each(&CsvBatch {
                records: full,
                index: &index,
                path,
            })?;
        }
    }

    if !records.is_empty() {
        each(&CsvBatch {
            records,
            index: &index,
            path,
        })?;
    }

    Ok(())
}
