//! Parquet datasets, read with arrow-rs: only the columns asked for, as typed Arrow arrays.

use std::fs::File;
use std::path::Path;

use arrow_array::cast::AsArray;
use arrow_array::types::{Date32Type, Float64Type, Int32Type, Int64Type, TimestampMicrosecondType};
use arrow_array::{Array, ArrayRef, RecordBatch};
use arrow_schema::{DataType, TimeUnit};
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

use rayon::prelude::*;

use crate::core::Kind;
use crate::storage::property::Cell;

use super::source::{BATCH_ROWS, Batch, Find, Key, LoadError, cell_from_text};


struct ParquetBatch<'p> {
    batch: RecordBatch,
    path: &'p Path,
}


impl ParquetBatch<'_> {
    fn column(&self, name: &str) -> Result<&ArrayRef, LoadError> {
        self.batch
            .column_by_name(name)
            .ok_or_else(|| LoadError::at(self.path, format!("no column {name}")))
    }


    /// The column as `to`, cast by Arrow when stored as something else.
    fn as_type(&self, name: &str, to: &DataType) -> Result<ArrayRef, LoadError> {
        let column = self.column(name)?;

        if column.data_type() == to {
            return Ok(column.clone());
        }

        arrow_cast::cast(column, to).map_err(|e| LoadError::at(self.path, format!("{name}: {e}")))
    }
}


impl Batch for ParquetBatch<'_> {
    fn rows(&self) -> usize {
        self.batch.num_rows()
    }


    fn each_key(
        &self,
        column: &str,
        each: &mut dyn FnMut(usize, Option<&[u8]>),
    ) -> Result<(), LoadError> {
        let array = self.column(column)?;
        let mut digits = [0u8; 20];

        match array.data_type() {
            DataType::Int64 => {
                let numbers = array.as_primitive::<Int64Type>();

                for row in 0..numbers.len() {
                    let key = numbers
                        .is_valid(row)
                        .then(|| decimal(numbers.value(row), &mut digits));

                    each(row, key);
                }
            }

            DataType::Int32 => {
                let numbers = array.as_primitive::<Int32Type>();

                for row in 0..numbers.len() {
                    let key = numbers
                        .is_valid(row)
                        .then(|| decimal(numbers.value(row) as i64, &mut digits));

                    each(row, key);
                }
            }

            _ => {
                let text = self.as_type(column, &DataType::Utf8)?;
                let text = text.as_string::<i32>();

                for row in 0..text.len() {
                    let key = text
                        .is_valid(row)
                        .then(|| text.value(row).as_bytes())
                        .filter(|key| !key.is_empty());

                    each(row, key);
                }
            }
        }

        Ok(())
    }


    fn lookup_keys(&self, column: &str, find: &Find) -> Result<Vec<Key>, LoadError> {
        let array = self.column(column)?;

        let keys = match array.data_type() {
            DataType::Int64 => {
                let numbers = array.as_primitive::<Int64Type>();

                (0..numbers.len())
                    .into_par_iter()
                    .map(|row| {
                        let mut digits = [0u8; 20];
                        let key = numbers
                            .is_valid(row)
                            .then(|| decimal(numbers.value(row), &mut digits));

                        Key::of(key, find)
                    })
                    .collect()
            }

            DataType::Int32 => {
                let numbers = array.as_primitive::<Int32Type>();

                (0..numbers.len())
                    .into_par_iter()
                    .map(|row| {
                        let mut digits = [0u8; 20];
                        let key = numbers
                            .is_valid(row)
                            .then(|| decimal(numbers.value(row) as i64, &mut digits));

                        Key::of(key, find)
                    })
                    .collect()
            }

            _ => {
                let text = self.as_type(column, &DataType::Utf8)?;
                let text = text.as_string::<i32>();

                (0..text.len())
                    .into_par_iter()
                    .map(|row| {
                        let key = text
                            .is_valid(row)
                            .then(|| text.value(row).as_bytes())
                            .filter(|key| !key.is_empty());

                        Key::of(key, find)
                    })
                    .collect()
            }
        };

        Ok(keys)
    }


    fn cells(&self, column: &str, kind: Kind) -> Result<Vec<Option<Cell>>, LoadError> {
        let stored_as_text = matches!(
            self.column(column)?.data_type(),
            DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View
        );

        // Dates and timestamps written as text are parsed the way CSV values are.
        if stored_as_text || kind == Kind::String {
            let text = self.as_type(column, &DataType::Utf8)?;
            let text = text.as_string::<i32>();

            return Ok((0..text.len())
                .map(|row| {
                    text.is_valid(row)
                        .then(|| cell_from_text(text.value(row), kind))
                        .flatten()
                })
                .collect());
        }

        let cells = match kind {
            Kind::String => unreachable!("handled as text above"),

            Kind::Integer | Kind::Long => {
                let numbers = self.as_type(column, &DataType::Int64)?;
                let numbers = numbers.as_primitive::<Int64Type>();

                each_valid(numbers, |row| Cell::Int(numbers.value(row)))
            }

            Kind::Double => {
                let numbers = self.as_type(column, &DataType::Float64)?;
                let numbers = numbers.as_primitive::<Float64Type>();

                each_valid(numbers, |row| Cell::F64(numbers.value(row)))
            }

            Kind::Boolean => {
                let flags = self.as_type(column, &DataType::Boolean)?;
                let flags = flags.as_boolean();

                each_valid(flags, |row| Cell::Bool(flags.value(row)))
            }

            Kind::Date => {
                let days = self.as_type(column, &DataType::Date32)?;
                let days = days.as_primitive::<Date32Type>();

                each_valid(days, |row| Cell::Int(days.value(row) as i64))
            }

            Kind::Timestamp => {
                let micros =
                    self.as_type(column, &DataType::Timestamp(TimeUnit::Microsecond, None))?;
                let micros = micros.as_primitive::<TimestampMicrosecondType>();

                each_valid(micros, |row| Cell::Int(micros.value(row)))
            }
        };

        Ok(cells)
    }
}


/// One cell per row: `value(row)` where the array has a value, `None` where it is null.
fn each_valid(array: &dyn Array, value: impl Fn(usize) -> Cell) -> Vec<Option<Cell>> {
    (0..array.len())
        .map(|row| array.is_valid(row).then(|| value(row)))
        .collect()
}


/// `n` in decimal, written into `digits`: the key bytes an integer key has in CSV.
fn decimal(n: i64, digits: &mut [u8; 20]) -> &[u8] {
    let negative = n < 0;
    let mut rest = n.unsigned_abs();
    let mut at = digits.len();

    loop {
        at -= 1;
        digits[at] = b'0' + (rest % 10) as u8;
        rest /= 10;

        if rest == 0 {
            break;
        }
    }

    if negative {
        at -= 1;
        digits[at] = b'-';
    }

    &digits[at..]
}


pub(crate) fn read(
    path: &Path,
    columns: &[&str],
    each: &mut dyn FnMut(&dyn Batch) -> Result<(), LoadError>,
) -> Result<(), LoadError> {
    let file = File::open(path).map_err(|e| LoadError::at(path, e))?;

    let builder =
        ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| LoadError::at(path, e))?;

    let names: Vec<String> = builder
        .schema()
        .fields()
        .iter()
        .map(|field| field.name().clone())
        .collect();

    for column in columns {
        if !names.iter().any(|name| name == column) {
            return Err(LoadError::at(path, format!("no column {column}")));
        }
    }

    let wanted = ProjectionMask::columns(builder.parquet_schema(), columns.iter().copied());

    let reader = builder
        .with_projection(wanted)
        .with_batch_size(BATCH_ROWS)
        .build()
        .map_err(|e| LoadError::at(path, e))?;

    for batch in reader {
        let batch = batch.map_err(|e| LoadError::at(path, e))?;

        each(&ParquetBatch { batch, path })?;
    }

    Ok(())
}
