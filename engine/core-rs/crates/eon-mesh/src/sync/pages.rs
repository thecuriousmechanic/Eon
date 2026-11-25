//! V4 pages carry one uncompressed, contiguous slice of a Loro change, not a
//! Loro import blob. Counts are checked before allocating and the only operation
//! representable is a root-map write/delete. No snapshot or dictionary decoding.

use eon_primitives::{EonError, EonResult};
use loro::{
    ContainerID, ContainerType, Frontiers, ID, IdSpan, JsonChange, JsonMapOp, JsonOp, JsonOpContent, JsonSchema,
    LoroValue, VersionVector,
};
use serde::{Deserialize, Serialize};

use super::{Replica, loro_err, projections, rejected};
use crate::transport::MAX_FRAME_BYTES;

const MAGIC: &[u8; 4] = b"EP4M";
const MAX_PAGE_OPS: u16 = 256;
pub(crate) const MAX_WRITERS: usize = 256;

pub(crate) fn check_writer_count(n: usize) -> EonResult<()> {
    if n > MAX_WRITERS {
        return Err(EonError::ResourceExhausted {
            resource: "paired frontier writers".into(),
            requested: n as u64,
            available: MAX_WRITERS as u64,
        });
    }

    Ok(())
}

pub(crate) enum Page {
    Current,
    Update { bytes: Vec<u8>, end: ID }, // exclusive end for this writer
    ItemTooLarge,
}

// Magic, u16 little-endian op count, postcard header, `deps` postcard IDs,
// then exactly `count` postcard writes. No message or opaque history fields.
#[derive(Serialize, Deserialize)]
struct Header {
    writer: u64,
    start: i32,
    timestamp: i64,
    lamport: u32,
    deps: u16,
}

#[derive(Serialize, Deserialize)]
struct Write<'a> {
    root: &'a str,
    key: &'a str,
    #[serde(borrow)]
    value: Option<&'a [u8]>,
}

fn take<'a, T: Deserialize<'a>>(input: &mut &'a [u8]) -> EonResult<T> {
    let (value, rest) = postcard::take_from_bytes(input).map_err(|_| rejected())?;
    *input = rest;
    Ok(value)
}

impl Replica {
    pub(crate) fn check_paired_frontier(&self, vv: &VersionVector) -> EonResult<()> {
        let frontiers = self.protected.try_vv_to_frontiers(vv).map_err(loro_err)?;
        if self
            .protected
            .try_frontiers_to_vv(&frontiers)
            .map_err(loro_err)?
            .as_ref()
            != Some(vv)
        {
            return Err(rejected());
        }
        Ok(())
    }

    pub(crate) fn paired_page(&self, from: &VersionVector, tip: &VersionVector, budget: usize) -> EonResult<Page> {
        if from.includes_vv(tip) {
            return Ok(Page::Current);
        }
        for missing in tip.sub_iter(from) {
            let writer = missing.peer;
            let start = missing.counter.start;
            let change = self
                .protected
                .try_get_change(ID::new(writer, start))
                .map_err(loro_err)?
                .ok_or_else(rejected)?;
            let deps = if start == change.id.counter {
                change.deps
            } else {
                Frontiers::from_id(ID::new(writer, start - 1))
            };
            if !deps.iter().all(|dep| from.includes_id(dep)) {
                continue;
            }
            if change.message.is_some() {
                return Err(rejected());
            }
            check_writer_count(deps.len())?;
            let header = Header {
                writer,
                start,
                timestamp: change.timestamp,
                lamport: change.lamport + (start - change.id.counter) as u32,
                deps: deps.len() as u16,
            };
            let mut bytes = MAGIC.to_vec();
            bytes.extend_from_slice(&0u16.to_le_bytes());
            bytes = postcard::to_extend(&header, bytes).map_err(loro_err)?;
            for id in deps.iter() {
                bytes = postcard::to_extend(&(id.peer, id.counter), bytes).map_err(loro_err)?;
            }
            let limit = missing
                .counter
                .end
                .min(change.id.counter + i32::try_from(change.len).map_err(loro_err)?)
                .min(start.saturating_add(i32::from(MAX_PAGE_OPS)));
            let mut end = start;
            while end < limit {
                // Materialize at most one map key; Loro binary values are shared.
                let ops = self.protected.export_json_in_id_span(IdSpan::new(writer, end, end + 1));
                let [part] = ops.as_slice() else { return Err(rejected()) };
                let [op] = part.ops.as_slice() else {
                    return Err(rejected());
                };
                let (
                    ContainerID::Root {
                        name,
                        container_type: ContainerType::Map,
                    },
                    JsonOpContent::Map(write),
                ) = (&op.container, &op.content)
                else {
                    return Err(rejected());
                };
                let (key, value) = match write {
                    JsonMapOp::Insert {
                        key,
                        value: LoroValue::Binary(v),
                    } => (key, Some(v.as_slice())),
                    JsonMapOp::Delete { key } => (key, None),
                    _ => return Err(rejected()),
                };
                let record = Write {
                    root: name.as_str(),
                    key,
                    value,
                };
                let remaining = budget.saturating_sub(bytes.len());
                if key.len().saturating_add(value.map_or(0, <[u8]>::len)) > remaining
                    || postcard::experimental::serialized_size(&record).map_err(loro_err)? > remaining
                {
                    break;
                }
                if !projections::allowed(record.root, write) {
                    return Err(rejected());
                }
                bytes = postcard::to_extend(&record, bytes).map_err(loro_err)?;
                end += 1;
            }
            if end == start {
                return Ok(Page::ItemTooLarge);
            }
            bytes[4..6].copy_from_slice(&((end - start) as u16).to_le_bytes());
            return Ok(Page::Update {
                bytes,
                end: ID::new(writer, end),
            });
        }
        Err(rejected())
    }

    /// Validate the complete change before its single atomic Loro import. For
    /// a pull, `from` is the VV saved before the RPC, not live import-time state.
    pub(crate) fn paired_apply_page(
        &self,
        bytes: &[u8],
        pull: Option<(&VersionVector, &VersionVector)>,
    ) -> EonResult<()> {
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(rejected());
        }
        let mut input = bytes.strip_prefix(MAGIC).ok_or_else(rejected)?;
        let count = u16::from_le_bytes(take(&mut input)?);
        let h: Header = take(&mut input)?;
        check_writer_count(usize::from(h.deps))?;
        if !(1..=MAX_PAGE_OPS).contains(&count) || h.start < 0 || h.writer == u64::MAX {
            return Err(rejected());
        }
        let end = h.start.checked_add(i32::from(count)).ok_or_else(rejected)?;
        h.lamport.checked_add(u32::from(count)).ok_or_else(rejected)?;
        let before = self.paired_version();
        check_writer_count(before.len() + usize::from(!before.contains_key(&h.writer)))?;
        let live_end = before.get(&h.writer).copied().unwrap_or(0);
        if live_end < h.start {
            return Err(rejected());
        }
        let mut deps = Frontiers::new();
        let mut next_lamport = 0;
        for _ in 0..h.deps {
            let (peer, counter) = take(&mut input)?;
            if counter < 0 || peer == u64::MAX {
                return Err(rejected());
            }
            let dep = ID::new(peer, counter);
            if !before.includes_id(dep) {
                return Err(rejected());
            }
            deps.push(dep);
            let c = self
                .protected
                .try_get_change(dep)
                .map_err(loro_err)?
                .ok_or_else(rejected)?;
            let next = c
                .lamport
                .checked_add((counter - c.id.counter) as u32 + 1)
                .ok_or_else(rejected)?;
            next_lamport = next_lamport.max(next);
        }
        if deps.len() != usize::from(h.deps) || h.lamport != next_lamport {
            return Err(rejected());
        }
        let mut closure = self
            .protected
            .try_frontiers_to_vv(&deps)
            .map_err(loro_err)?
            .ok_or_else(rejected)?;
        if closure.get(&h.writer).copied().unwrap_or(0) != h.start {
            return Err(rejected());
        }
        if live_end > h.start {
            let overlap = live_end.min(end);
            let last = Frontiers::from_id(ID::new(h.writer, overlap - 1));
            let mut expected = closure.clone();
            expected.set_end(ID::new(h.writer, overlap));
            if self.protected.try_frontiers_to_vv(&last).map_err(loro_err)?.as_ref() != Some(&expected) {
                return Err(rejected());
            }
        }
        if let Some((from, target)) = pull {
            if from.get(&h.writer).copied().unwrap_or(0) != h.start || !from.includes_vv(&closure) {
                return Err(rejected());
            }
            closure.set_end(ID::new(h.writer, end));
            if !target.includes_vv(&closure) {
                return Err(rejected());
            }
            // The common part of two real, causally closed histories is itself
            // a valid historical cut, even if the whole target isn't here yet.
            self.check_paired_frontier(&before.intersection(target))?;
        }
        let mut ops = Vec::with_capacity(usize::from(count));
        for counter in h.start..end {
            let w: Write = take(&mut input)?;
            let content = match w.value {
                None => JsonMapOp::Delete { key: w.key.into() },
                Some(bytes) => JsonMapOp::Insert {
                    key: w.key.into(),
                    value: LoroValue::from(bytes.to_vec()),
                },
            };
            if !projections::allowed(w.root, &content) {
                return Err(rejected());
            }
            ops.push(JsonOp {
                content: JsonOpContent::Map(content),
                container: ContainerID::new_root(w.root, ContainerType::Map),
                counter,
            });
        }
        if !input.is_empty() {
            return Err(rejected());
        }
        let change = JsonChange {
            id: ID::new(h.writer, h.start),
            deps: deps.iter().collect(),
            lamport: h.lamport,
            timestamp: h.timestamp,
            msg: None,
            ops,
        };
        let status = self
            .protected
            .import_json_updates(JsonSchema {
                schema_version: 1,
                start_version: deps,
                peers: None,
                changes: vec![change],
            })
            .map_err(|_| rejected())?;
        if status.pending.is_some() || !self.paired_version().includes_id(ID::new(h.writer, end - 1)) {
            return Err(rejected());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::Entity;
    use loro::{ExportMode, LoroDoc};

    #[test]
    fn paired_pages_refuse_blob_modes_bad_counts_ids_and_truncated_writes_atomically() {
        let a = Replica::new(1).unwrap();
        let b = Replica::new(2).unwrap();
        a.put_entity(
            "k",
            &Entity {
                kind: "doc".into(),
                label: "x".into(),
            },
        )
        .unwrap();
        let from = b.paired_version();
        let tip = a.paired_version();
        assert!(matches!(a.paired_page(&from, &tip, 1).unwrap(), Page::ItemTooLarge));
        let Page::Update { bytes, .. } = a.paired_page(&from, &tip, MAX_FRAME_BYTES).unwrap() else {
            panic!()
        };
        let doc = LoroDoc::new();
        let mut bad_pages = vec![
            doc.export(ExportMode::Snapshot).unwrap(),
            doc.export(ExportMode::all_updates()).unwrap(),
            vec![0; MAX_FRAME_BYTES + 1],
        ];
        for count in [0, 2, MAX_PAGE_OPS + 1] {
            let mut bad = bytes.clone();
            bad[4..6].copy_from_slice(&count.to_le_bytes());
            bad_pages.push(bad); // count=2 must not apply the valid first write
        }
        let mut body = &bytes[6..];
        let mut header: Header = take(&mut body).unwrap();
        for dep in [(1_u64, -1_i32), (u64::MAX, 0)] {
            header.deps = 1;
            let bad = postcard::to_extend(&header, bytes[..6].to_vec()).unwrap();
            let mut bad = postcard::to_extend(&dep, bad).unwrap();
            bad.extend_from_slice(body);
            bad_pages.push(bad);
        }
        header.deps = 0;
        header.writer = u64::MAX;
        let mut bad = postcard::to_extend(&header, bytes[..6].to_vec()).unwrap();
        bad.extend_from_slice(body);
        bad_pages.push(bad);
        for bad in bad_pages {
            assert!(b.paired_apply_page(&bad, None).is_err());
            assert_eq!(b.paired_version(), from);
        }
    }
}
