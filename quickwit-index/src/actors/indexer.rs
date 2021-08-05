// Quickwit
//  Copyright (C) 2021 Quickwit Inc.
//
//  Quickwit is offered under the AGPL v3.0 and as commercial software.
//  For commercial licensing, contact us at hello@quickwit.io.
//
//  AGPL:
//  This program is free software: you can redistribute it and/or modify
//  it under the terms of the GNU Affero General Public License as
//  published by the Free Software Foundation, either version 3 of the
//  License, or (at your option) any later version.
//
//  This program is distributed in the hope that it will be useful,
//  but WITHOUT ANY WARRANTY; without even the implied warranty of
//  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
//  GNU Affero General Public License for more details.
//
//  You should have received a copy of the GNU Affero General Public License
//  along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::io;
use std::ops::RangeInclusive;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use quickwit_actors::Actor;
use quickwit_actors::Mailbox;
use quickwit_actors::SendError;
use quickwit_actors::SyncActor;
use quickwit_index_config::IndexConfig;
use tantivy::schema::Field;
use tantivy::Document;
use tracing::warn;

use crate::models::IndexedSplit;
use crate::models::RawDocBatch;

#[derive(Clone, Default, Debug)]
pub struct IndexerCounters {
    parse_error: u64,
    docs: u64,
}

enum ScratchDirectory {
    Path(PathBuf),
    TempDir(tempfile::TempDir),
}

impl ScratchDirectory {
    fn try_new_temp() -> io::Result<ScratchDirectory> {
        let temp_dir = tempfile::tempdir()?;
        Ok(ScratchDirectory::TempDir(temp_dir))
    }
    fn path(&self) -> &Path {
        match self {
            ScratchDirectory::Path(path) => path,
            ScratchDirectory::TempDir(tempdir) => tempdir.path(),
        }
    }
}

pub struct Indexer {
    index_id: String,
    index_config: Arc<dyn IndexConfig>,
    // splits index writer will write in TempDir within this directory
    indexing_scratch_directory: ScratchDirectory,
    commit_timeout: Duration,
    sink: Mailbox<IndexedSplit>,
    next_commit_deadline_opt: Option<Instant>,
    current_split_opt: Option<IndexedSplit>,
    counters: IndexerCounters,
    timestamp_field_opt: Option<Field>,
}

impl Actor for Indexer {
    type Message = RawDocBatch;

    type ObservableState = IndexerCounters;

    fn observable_state(&self) -> Self::ObservableState {
        self.counters.clone()
    }
}

fn extract_timestamp(doc: &Document, timestamp_field_opt: Option<Field>) -> Option<i64> {
    let timestamp_field = timestamp_field_opt?;
    let timestamp_value = doc.get_first(timestamp_field)?;
    timestamp_value.i64_value()
}

impl SyncActor for Indexer {
    fn process_message(
        &mut self,
        batch: RawDocBatch,
        _context: quickwit_actors::ActorContext<'_, Self::Message>,
    ) -> Result<(), quickwit_actors::MessageProcessError> {
        let index_config = self.index_config.clone();
        let timestamp_field_opt = self.timestamp_field_opt;
        let indexed_split = self.indexed_split()?;
        for doc_json in batch.docs {
            indexed_split.size_in_bytes += doc_json.len() as u64;
            let doc_parsing_result = index_config.doc_from_json(&doc_json);
            let doc = match doc_parsing_result {
                Ok(doc) => doc,
                Err(doc_parsing_error) => {
                    // TODO we should at least keep track of the number of parse error.
                    warn!(err=?doc_parsing_error);
                    continue;
                }
            };
            if let Some(timestamp) = extract_timestamp(&doc, timestamp_field_opt) {
                let new_timestamp_range = match indexed_split.time_range.as_ref() {
                    Some(range) => RangeInclusive::new(
                        timestamp.min(*range.start()),
                        timestamp.max(*range.end()),
                    ),
                    None => RangeInclusive::new(timestamp, timestamp),
                };
                indexed_split.time_range = Some(new_timestamp_range);
            }
            indexed_split.index_writer.add_document(doc);
        }

        // TODO this approach of deadline is not correct, as it never triggers if no need
        // new message arrives.
        // We do need to implement timeout message in actors to get the right behavior.
        if let Some(deadline) = self.next_commit_deadline_opt {
            let now = Instant::now();
            if now >= deadline {
                self.send_to_packager()?;
            }
        } else {
            self.next_commit_deadline_opt = None;
        }
        Ok(())
    }
}

impl Indexer {
    // TODO take all of the parameter and dispatch them in index config, or in a different
    // IndexerParams object.
    pub fn try_new(
        index_id: String,
        index_config: Arc<dyn IndexConfig>,
        indexing_directory: Option<PathBuf>, //< if None, we create a tempdirectory.
        commit_timeout: Duration,
        sink: Mailbox<IndexedSplit>,
    ) -> anyhow::Result<Indexer> {
        let indexing_scratch_directory = if let Some(path) = indexing_directory {
            ScratchDirectory::Path(path)
        } else {
            ScratchDirectory::try_new_temp()?
        };
        let time_field_opt = index_config.timestamp_field();
        Ok(Indexer {
            index_id,
            index_config,
            commit_timeout,
            sink,
            next_commit_deadline_opt: None,
            counters: IndexerCounters::default(),
            current_split_opt: None,
            indexing_scratch_directory,
            timestamp_field_opt: time_field_opt,
        })
    }

    fn create_indexed_split(&self) -> anyhow::Result<IndexedSplit> {
        let schema = self.index_config.schema();
        let indexed_split = IndexedSplit::new_in_dir(
            self.index_id.clone(),
            self.indexing_scratch_directory.path(),
            schema,
        )?;
        Ok(indexed_split)
    }

    fn indexed_split(&mut self) -> anyhow::Result<&mut IndexedSplit> {
        if self.current_split_opt.is_none() {
            let new_indexed_split = self.create_indexed_split()?;
            self.current_split_opt = Some(new_indexed_split);
            self.next_commit_deadline_opt = Some(Instant::now() + self.commit_timeout);
        }
        let current_index_split = self.current_split_opt.as_mut().with_context(|| {
            "No index writer available. Please report: this should never happen."
        })?;
        Ok(current_index_split)
    }

    fn send_to_packager(&mut self) -> Result<(), SendError> {
        let indexed_split = if let Some(indexed_split) = self.current_split_opt.take() {
            indexed_split
        } else {
            return Ok(());
        };
        self.sink.send_blocking(indexed_split)?;
        Ok(())
    }
}