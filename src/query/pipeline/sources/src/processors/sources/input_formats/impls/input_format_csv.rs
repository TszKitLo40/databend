//  Copyright 2022 Datafuse Labs.
//
//  Licensed under the Apache License, Version 2.0 (the "License");
//  you may not use this file except in compliance with the License.
//  You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
//  Unless required by applicable law or agreed to in writing, software
//  distributed under the License is distributed on an "AS IS" BASIS,
//  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//  See the License for the specific language governing permissions and
//  limitations under the License.

use std::mem;
use std::sync::Arc;

use common_datavalues::DataSchemaRef;
use common_datavalues::TypeDeserializer;
use common_exception::ErrorCode;
use common_exception::Result;
use common_io::prelude::BufferReadExt;
use common_io::prelude::FormatSettings;
use common_io::prelude::NestedCheckpointReader;
use common_meta_types::StageFileFormatType;
use common_settings::Settings;
use csv_core::ReadRecordResult;

use crate::processors::sources::input_formats::delimiter::RecordDelimiter;
use crate::processors::sources::input_formats::impls::input_format_tsv::format_column_error;
use crate::processors::sources::input_formats::input_format_text::get_time_zone;
use crate::processors::sources::input_formats::input_format_text::AligningState;
use crate::processors::sources::input_formats::input_format_text::BlockBuilder;
use crate::processors::sources::input_formats::input_format_text::InputFormatTextBase;
use crate::processors::sources::input_formats::input_format_text::RowBatch;
use crate::processors::sources::input_formats::InputContext;

pub struct InputFormatCSV {}

impl InputFormatCSV {
    fn read_row(
        buf: &[u8],
        deserializers: &mut [common_datavalues::TypeDeserializerImpl],
        schema: &DataSchemaRef,
        field_ends: &[usize],
        format_settings: &FormatSettings,
        path: &str,
        row_index: usize,
    ) -> Result<()> {
        let mut field_start = 0;
        for (c, deserializer) in deserializers.iter_mut().enumerate() {
            let field_end = field_ends[c];
            let col_data = &buf[field_start..field_end];
            let mut reader = NestedCheckpointReader::new(col_data);
            reader.ignore_white_spaces().expect("must success");
            if reader.eof().expect("must success") {
                deserializer.de_default(format_settings);
            } else {
                // todo(youngsofun): do not need escape, already done in csv-core
                if let Err(e) = deserializer.de_text(&mut reader, format_settings) {
                    let err_msg = format_column_error(schema, c, col_data, &e.message());
                    return Err(csv_error(&err_msg, path, row_index));
                };
                reader.ignore_white_spaces().expect("must success");
                if reader.must_eof().is_err() {
                    let err_msg = format_column_error(schema, c, col_data, "bad field end");
                    return Err(csv_error(&err_msg, path, row_index));
                }
            }
            field_start = field_end;
        }
        Ok(())
    }
}

impl InputFormatTextBase for InputFormatCSV {
    fn format_type() -> StageFileFormatType {
        StageFileFormatType::Csv
    }

    fn get_format_settings(settings: &Arc<Settings>) -> Result<FormatSettings> {
        let timezone = get_time_zone(settings)?;
        let quote_char = settings.get_format_quote_char()?.into_bytes();
        if quote_char.len() != 1 {
            return Err(ErrorCode::InvalidArgument(
                "quote_char can only contain one char",
            ));
        }
        Ok(FormatSettings {
            record_delimiter: settings.get_format_record_delimiter()?.into_bytes(),
            field_delimiter: settings.get_format_field_delimiter()?.into_bytes(),
            empty_as_default: settings.get_format_empty_as_default()? > 0,
            quote_char: quote_char[0],
            null_bytes: vec![b'\\', b'N'],
            timezone,
            ..Default::default()
        })
    }

    fn default_field_delimiter() -> u8 {
        b','
    }

    fn deserialize(builder: &mut BlockBuilder<Self>, batch: RowBatch) -> Result<()> {
        let columns = &mut builder.mutable_columns;
        let n_column = columns.len();
        let mut start = 0usize;
        let start_row = batch.start_row.expect("must success");
        let mut field_end_idx = 0;
        for (i, end) in batch.row_ends.iter().enumerate() {
            let buf = &batch.data[start..*end];
            Self::read_row(
                buf,
                columns,
                &builder.ctx.schema,
                &batch.field_ends[field_end_idx..field_end_idx + n_column],
                &builder.ctx.format_settings,
                &batch.path,
                start_row + i,
            )?;
            start = *end;
            field_end_idx += n_column;
        }
        Ok(())
    }

    fn align(state: &mut AligningState<Self>, buf_in: &[u8]) -> Result<Vec<RowBatch>> {
        let num_fields = state.num_fields;
        let reader = state.csv_reader.as_mut().expect("must success");
        let field_ends = &mut reader.field_ends[..];
        let start_row = state.rows;
        state.offset += buf_in.len();

        // assume n_out <= n_in for read_record
        let mut out_tmp = vec![0u8; buf_in.len()];
        let mut endlen = reader.n_end;
        let mut buf = buf_in;

        while state.rows_to_skip > 0 {
            let (result, n_in, _, n_end) =
                reader
                    .reader
                    .read_record(buf, &mut out_tmp, &mut field_ends[endlen..]);
            buf = &buf[n_in..];
            endlen += n_end;

            match result {
                ReadRecordResult::InputEmpty => {
                    reader.n_end = endlen;
                    return Ok(vec![]);
                }
                ReadRecordResult::OutputFull => {
                    return Err(csv_error(
                        "output more than input, in header",
                        &state.path,
                        state.rows,
                    ));
                }
                ReadRecordResult::OutputEndsFull => {
                    return Err(csv_error(
                        &format!(
                            "too many fields, expect {}, got more than {}",
                            num_fields,
                            field_ends.len()
                        ),
                        &state.path,
                        state.rows,
                    ));
                }
                ReadRecordResult::Record => {
                    if endlen < num_fields {
                        return Err(csv_error(
                            &format!("expect {} fields, only found {} ", num_fields, n_end),
                            &state.path,
                            state.rows,
                        ));
                    } else if endlen > num_fields + 1 {
                        return Err(csv_error(
                            &format!("too many fields, expect {}, got {}", num_fields, n_end),
                            &state.path,
                            state.rows,
                        ));
                    } else if endlen == num_fields + 1
                        && field_ends[num_fields] != field_ends[num_fields - 1]
                    {
                        return Err(csv_error(
                            "CSV allow ending with ',', but should not have data after it",
                            &state.path,
                            state.rows,
                        ));
                    }

                    state.rows_to_skip -= 1;
                    tracing::debug!(
                        "csv aligner: skip a header row, remain {}",
                        state.rows_to_skip
                    );
                    state.rows += 1;
                    endlen = 0;
                }
                ReadRecordResult::End => {
                    return Err(csv_error("unexpect eof in header", &state.path, state.rows));
                }
            }
        }

        let mut out_pos = 0usize;
        let mut row_batch_end: usize = 0;

        let last_batch_remain_len = reader.out.len();

        let mut row_batch = RowBatch {
            data: vec![],
            row_ends: vec![],
            field_ends: vec![],
            path: state.path.to_string(),
            batch_id: state.batch_id,
            offset: 0,
            start_row: Some(state.rows),
        };

        while !buf.is_empty() {
            let (result, n_in, n_out, n_end) =
                reader
                    .reader
                    .read_record(buf, &mut out_tmp[out_pos..], &mut field_ends[endlen..]);
            buf = &buf[n_in..];
            endlen += n_end;
            out_pos += n_out;
            match result {
                ReadRecordResult::InputEmpty => break,
                ReadRecordResult::OutputFull => {
                    return Err(csv_error(
                        "output more than input",
                        &state.path,
                        start_row + row_batch.row_ends.len(),
                    ));
                }
                ReadRecordResult::OutputEndsFull => {
                    return Err(csv_error(
                        &format!(
                            "too many fields, expect {}, got more than {}",
                            num_fields,
                            field_ends.len()
                        ),
                        &state.path,
                        start_row + row_batch.row_ends.len(),
                    ));
                }
                ReadRecordResult::Record => {
                    if endlen < num_fields {
                        return Err(csv_error(
                            &format!("expect {} fields, only found {} ", num_fields, n_end),
                            &state.path,
                            start_row + row_batch.row_ends.len(),
                        ));
                    } else if endlen > num_fields + 1 {
                        return Err(csv_error(
                            &format!("too many fields, expect {}, got {}", num_fields, n_end),
                            &state.path,
                            start_row + row_batch.row_ends.len(),
                        ));
                    } else if endlen == num_fields + 1
                        && field_ends[num_fields] != field_ends[num_fields - 1]
                    {
                        return Err(csv_error(
                            "CSV allow ending with ',', but should not have data after it",
                            &state.path,
                            start_row + row_batch.row_ends.len(),
                        ));
                    }
                    row_batch
                        .field_ends
                        .extend_from_slice(&field_ends[..num_fields]);
                    row_batch.row_ends.push(last_batch_remain_len + out_pos);
                    endlen = 0;
                    row_batch_end = out_pos;
                }
                ReadRecordResult::End => {
                    return Err(csv_error(
                        "unexpect eof",
                        &state.path,
                        start_row + row_batch.row_ends.len(),
                    ));
                }
            }
        }

        reader.n_end = endlen;
        out_tmp.truncate(out_pos);
        if row_batch.row_ends.is_empty() {
            tracing::debug!(
                "csv aligner: {} + {} bytes => 0 rows",
                reader.out.len(),
                buf_in.len(),
            );
            reader.out.extend_from_slice(&out_tmp);
            Ok(vec![])
        } else {
            let last_remain = mem::take(&mut reader.out);

            state.batch_id += 1;
            state.rows += row_batch.row_ends.len();
            reader.out.extend_from_slice(&out_tmp[row_batch_end..]);

            tracing::debug!(
                "csv aligner: {} + {} bytes => {} rows + {} bytes remain",
                last_remain.len(),
                buf_in.len(),
                row_batch.row_ends.len(),
                reader.out.len()
            );

            out_tmp.truncate(row_batch_end);
            row_batch.data = if last_remain.is_empty() {
                out_tmp
            } else {
                vec![last_remain, out_tmp].concat()
            };
            Ok(vec![row_batch])
        }
    }
}

pub struct CsvReaderState {
    pub reader: csv_core::Reader,

    // remain from last read batch
    pub out: Vec<u8>,
    pub field_ends: Vec<usize>,
    pub n_end: usize,
}

impl CsvReaderState {
    pub(crate) fn create(ctx: &Arc<InputContext>) -> Self {
        let reader = csv_core::ReaderBuilder::new()
            .delimiter(ctx.field_delimiter)
            .quote(ctx.format_settings.quote_char)
            .terminator(match ctx.record_delimiter {
                RecordDelimiter::Crlf => csv_core::Terminator::CRLF,
                RecordDelimiter::Any(v) => csv_core::Terminator::Any(v),
            })
            .build();
        Self {
            reader,
            out: vec![],
            field_ends: vec![0; ctx.schema.num_fields() + 6],
            n_end: 0,
        }
    }
}

fn csv_error(msg: &str, path: &str, row: usize) -> ErrorCode {
    let row = row + 1;
    let msg = format!("fail to parse CSV {}:{} {} ", path, row, msg);

    ErrorCode::BadBytes(msg)
}
