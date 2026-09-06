//! Verification over a positioned source, in declared units.
//!
//! [`super::verify_container_artifact`] proves a container whole from a filesystem handle in one
//! pass. A browser tab or an edge worker reading a store by HTTP range cannot pay for that shape:
//! the browser core answers a cache miss by aborting the operation and retrying it once the bytes
//! are fetched, so a single pass that touches every byte of a store larger than its cache would
//! restart on every miss; and a store can be larger than the memory available to hold it whole.
//!
//! [`SourceVerifier`] performs the same checks — every member's recorded checksum, the retained
//! manifest chain, every manifest's part digest, every part section's checksum, every physical
//! row's grammar, every operational piece-dictionary entry against its fold bytes, every fold
//! frame, every live named content value's identity, and every retained authority's content — as
//! an ordered list of [`VerifyUnit`]s. Each unit declares its byte [`SourceVerifier::footprint`]
//! before it [`SourceVerifier::run`]s, so a host fetches exactly those ranges first and the unit
//! completes in one pass; a unit that reads more than its footprint is a bug this module's tests
//! catch by recording every read.
//!
//! The evidence composes: once every unit has run, [`SourceVerifier::report`] returns the same
//! [`StoreVerification`] the whole-artifact path produces. Before that, the evidence is scoped to
//! the units that ran — a member-level verification result is not a whole-store result — and
//! `report` refuses rather than claim more.
//!
//! Windows over long members (member checksums, part digests, fold frames) carry hasher state
//! between units and therefore run in order. A window that fails — including a source that
//! reports a missing range — leaves the hasher exactly as it was, so the host can fetch and rerun
//! that window alone.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};

use super::{
    container_retained_commits, open_read_container_handle, open_retained_from_container,
    verified_punched_fold_members, verify_chain_container_scoped, ChainReport, Manifest, ReadStore,
    StoreVerification, MAX_MANIFEST_BYTES,
};
use crate::container::{ContainerReader, ContainerView};
use crate::control::OperationControl;
use crate::fold::{segment, Fold, FoldCfg, FoldScrub};
use crate::part::Part;
use crate::read_limits::ReadLimits;
use crate::readat::{physical_ranges, ReadAt};
use crate::types::ContentOp;

/// Bytes a windowed unit covers at most. Large enough that a host's per-unit fetch is worth its
/// round trip, small enough that a browser tab or a 128 MiB edge isolate holds a unit's footprint
/// comfortably beside the engine's own caches. A single fold frame larger than this is its own
/// window because a frame verifies whole or not at all.
pub const WINDOW_BYTES: u64 = 8 << 20;

/// Rows one content unit covers at most. Rows rather than bytes because sizing by bytes would
/// require decoding every content program at planning time, and planning must stay a metadata
/// act that a cold range source can afford.
pub const CONTENT_ROWS: usize = 256;

/// Which store authority a unit's content or piece-dictionary claim is made against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Authority {
    /// The manifest revision selected by the current container state.
    Current,
    /// A retained manifest revision, by its `commit` counter.
    Retained(u64),
}

/// One bounded verification act. The variants are the whole-artifact verification's checks,
/// each cut at the boundary a range source can fill.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerifyUnit {
    /// One window of a member's logical bytes into the crc32 the container directory recorded.
    /// Members exempt from this check — fold segments whose payloads the current manifest
    /// declares punched — receive no such unit; their frames are still verified.
    MemberChecksum { member: String, start: u64, stop: u64 },
    /// The retained manifest revisions parse, chain by predecessor digest, keep their cursors and
    /// fold tails ordered, name only members the container holds, and agree with `MANIFEST`.
    /// Reads no part bytes; part pins are [`VerifyUnit::PartDigest`]'s.
    ManifestChain,
    /// One window of a part member into the BLAKE3 digest every manifest revision naming the
    /// member pinned.
    PartDigest { member: String, start: u64, stop: u64 },
    /// One section's stored bytes against the checksum the part's table of contents recorded.
    PartSection { member: String, section: String },
    /// Every physical row and piece-dictionary entry of the part decodes under the current grammar.
    PartGrammar { member: String },
    /// Every operational piece-dictionary entry of the part resolves to fold bytes carrying its
    /// identity, under the named authority's fold view.
    PartPieces { member: String, authority: Authority },
    /// The fold frames in one window `[start, stop)` of one segment, at frame boundaries.
    FoldFrames { seg: u32, start: u64, stop: u64 },
    /// Every named content value of the rows `rows` of the authority's part `part` (indexes into
    /// the rows visible under that authority, not physical rows) reconstructs byte-exactly to
    /// its recorded identity.
    Content { authority: Authority, part: usize, rows: Range<usize> },
}

impl VerifyUnit {
    /// The unit's kind as a stable lower-camel word, for hosts that report progress.
    pub fn kind(&self) -> &'static str {
        match self {
            VerifyUnit::MemberChecksum { .. } => "memberChecksum",
            VerifyUnit::ManifestChain => "manifestChain",
            VerifyUnit::PartDigest { .. } => "partDigest",
            VerifyUnit::PartSection { .. } => "partSection",
            VerifyUnit::PartGrammar { .. } => "partGrammar",
            VerifyUnit::PartPieces { .. } => "partPieces",
            VerifyUnit::FoldFrames { .. } => "foldFrames",
            VerifyUnit::Content { .. } => "content",
        }
    }
}

/// What a completed verification of a source-backed container established.
#[derive(Clone, Copy, Debug)]
pub struct SourceVerification {
    /// Members in the selected container directory.
    pub members: usize,
    /// Bytes those members occupy.
    pub member_bytes: u64,
    /// The `commit` counter of the current store authority; zero encodes the canonical origin.
    pub commit: u64,
    pub store: StoreVerification,
}

/// One authority's opened view and the rows visible under it, per part.
struct AuthorityView {
    store: ReadStore,
    /// Physical rows resolving to a present record, per part, in row order.
    visible: Vec<Vec<usize>>,
    /// `(segment, offset, frame length)` per block id, or `None` for a block this authority's
    /// fold cannot address.
    frames: Vec<Option<(u32, u64, u64)>>,
}

/// A verification in progress over one source-backed container.
pub struct SourceVerifier {
    container: ContainerReader,
    label: PathBuf,
    cfg: FoldCfg,
    read_limits: ReadLimits,
    current: AuthorityView,
    retained: Vec<(u64, AuthorityView)>,
    fold_prefix: String,
    /// Part member name → the digest every manifest revision naming it pinned, with the revision
    /// (`0` for the current manifest of a store retaining no history).
    pins: BTreeMap<String, Vec<(u64, String)>>,
    /// Every part member any manifest names, opened once, whichever authorities share it.
    parts: BTreeMap<String, Arc<Part>>,
    units: Vec<VerifyUnit>,
    done: Vec<bool>,
    member_crc: HashMap<String, (u64, crc32fast::Hasher)>,
    part_blake: HashMap<String, (u64, blake3::Hasher)>,
    fold_windows_done: HashMap<u32, u64>,
    chain: Option<ChainReport>,
    part_digests: usize,
    fold: FoldScrub,
    segments_complete: HashSet<u32>,
    part_sections: usize,
    records: usize,
    content_values: usize,
    content_bytes: u64,
    content_identities: usize,
}

impl SourceVerifier {
    /// Open a container over `source` and plan its verification. This reads metadata only: the
    /// superblocks and directory, every manifest, every part's footer and table of contents, the
    /// `ids` column of every part (to resolve visibility), and every fold segment's header and
    /// advisory directory. When the current manifest declares punched blocks, the fold is
    /// additionally scrubbed here to classify which segment members are exempt from the outer
    /// checksum — a whole-fold read that a punched store pays at open rather than in a unit.
    pub fn open(
        source: Arc<dyn ReadAt>,
        label: &str,
        cfg: FoldCfg,
        read_limits: ReadLimits,
        control: &OperationControl,
    ) -> Result<SourceVerifier> {
        let read_limits = read_limits.validate()?;
        crate::fold::validate_cfg(cfg)?;
        let container = integrity(
            "open container for verification",
            ContainerReader::open_with_limits(source, label, read_limits),
        )?;
        let label_path = PathBuf::from(label);
        read_limits.admit_directory_entries(
            format!("container {label} member directory"),
            container.member_count() as u64,
        )?;
        let store = integrity(
            "open container store for verification",
            open_read_container_handle(&container, cfg, &label_path, read_limits),
        )?;
        let current_manifest = store.manifest.clone();
        let fold_prefix = crate::fold::fold_member_prefix(current_manifest.fold_gen);
        let ignored = integrity(
            "classify punched fold members",
            verified_punched_fold_members(&container, cfg, read_limits, control),
        )?;
        let current = AuthorityView::open(store, control)?;

        let names = container.member_names();
        let mut retained = Vec::new();
        let mut pins: BTreeMap<String, Vec<(u64, String)>> = BTreeMap::new();
        let commits = container_retained_commits(&container);
        for &commit in &commits {
            control.check("store verification")?;
            let manifest = Manifest::parse(
                &container
                    .read_file_bounded(&format!("MANIFEST.{commit:08}"), MAX_MANIFEST_BYTES)?,
            )?;
            for part in &manifest.parts {
                pins.entry(part.member.clone()).or_default().push((commit, part.b3.clone()));
            }
            let view = integrity(
                "open retained authority for verification",
                open_retained_from_container(
                    &container,
                    &label_path,
                    cfg,
                    commit,
                    manifest,
                    &current_manifest,
                    names.clone(),
                    read_limits,
                ),
            )?;
            retained.push((commit, AuthorityView::open(view, control)?));
        }
        if commits.is_empty() {
            for part in &current_manifest.parts {
                pins.entry(part.member.clone()).or_default().push((0, part.b3.clone()));
            }
        }

        let mut parts: BTreeMap<String, Arc<Part>> = BTreeMap::new();
        for (index, part) in current_manifest.parts.iter().enumerate() {
            parts.insert(part.member.clone(), current.store.parts[index].clone());
        }
        for (_, view) in &retained {
            for (index, part) in view.store.manifest.parts.iter().enumerate() {
                parts.entry(part.member.clone()).or_insert_with(|| view.store.parts[index].clone());
            }
        }

        let mut verifier = SourceVerifier {
            container,
            label: label_path,
            cfg,
            read_limits,
            current,
            retained,
            fold_prefix,
            pins,
            parts,
            units: Vec::new(),
            done: Vec::new(),
            member_crc: HashMap::new(),
            part_blake: HashMap::new(),
            fold_windows_done: HashMap::new(),
            chain: None,
            part_digests: 0,
            fold: FoldScrub::default(),
            segments_complete: HashSet::new(),
            part_sections: 0,
            records: 0,
            content_values: 0,
            content_bytes: 0,
            content_identities: 0,
        };
        verifier.units = verifier.plan(&ignored)?;
        verifier.done = vec![false; verifier.units.len()];
        Ok(verifier)
    }

    fn plan(&self, ignored: &HashSet<String>) -> Result<Vec<VerifyUnit>> {
        let mut units = Vec::new();
        for name in self.container.member_names() {
            if ignored.contains(&name) {
                continue;
            }
            let len = self.container.member_len(&name).expect("name came from the directory");
            for (start, stop) in windows(0, len) {
                units.push(VerifyUnit::MemberChecksum { member: name.clone(), start, stop });
            }
        }
        units.push(VerifyUnit::ManifestChain);
        for (member, part) in &self.parts {
            let len = part.byte_len()?;
            for (start, stop) in windows(0, len) {
                units.push(VerifyUnit::PartDigest { member: member.clone(), start, stop });
            }
            for (section, _, _) in part.section_ranges() {
                units.push(VerifyUnit::PartSection { member: member.clone(), section });
            }
            units.push(VerifyUnit::PartGrammar { member: member.clone() });
        }
        for part in &self.current.store.manifest.parts {
            units.push(VerifyUnit::PartPieces {
                member: part.member.clone(),
                authority: Authority::Current,
            });
        }
        for seg in 0..self.current.store.fold.segment_count() {
            let len = self.current.store.fold.segment_len(seg)?;
            let mut boundaries: Vec<u64> = self
                .current
                .frames
                .iter()
                .flatten()
                .filter(|(s, _, _)| *s == seg)
                .map(|(_, off, _)| *off)
                .collect();
            boundaries.sort_unstable();
            boundaries.dedup();
            let mut window_start = segment::SEG_HDR_LEN;
            for boundary in boundaries.iter().skip_while(|b| **b <= segment::SEG_HDR_LEN) {
                if *boundary - window_start >= WINDOW_BYTES {
                    units.push(VerifyUnit::FoldFrames {
                        seg,
                        start: window_start,
                        stop: *boundary,
                    });
                    window_start = *boundary;
                }
            }
            // The last window always exists, even over an empty segment, so the tail condition
            // — frames dense to the segment's end — is judged for every segment.
            units.push(VerifyUnit::FoldFrames {
                seg,
                start: window_start,
                stop: len.max(window_start),
            });
        }
        for (index, rows) in self.current.visible.iter().enumerate() {
            for range in row_windows(rows.len()) {
                units.push(VerifyUnit::Content {
                    authority: Authority::Current,
                    part: index,
                    rows: range,
                });
            }
        }
        for (commit, view) in &self.retained {
            for part in &view.store.manifest.parts {
                units.push(VerifyUnit::PartPieces {
                    member: part.member.clone(),
                    authority: Authority::Retained(*commit),
                });
            }
            for (index, rows) in view.visible.iter().enumerate() {
                for range in row_windows(rows.len()) {
                    units.push(VerifyUnit::Content {
                        authority: Authority::Retained(*commit),
                        part: index,
                        rows: range,
                    });
                }
            }
        }
        Ok(units)
    }

    /// The planned units, in the order windows must run.
    pub fn units(&self) -> &[VerifyUnit] {
        &self.units
    }

    /// How many units have run, and how many were planned.
    pub fn progress(&self) -> (usize, usize) {
        (self.done.iter().filter(|done| **done).count(), self.units.len())
    }

    /// The commit counter of the current store authority.
    pub fn commit(&self) -> u64 {
        self.current.store.manifest.commit
    }

    fn authority(&self, authority: Authority) -> Result<&AuthorityView> {
        match authority {
            Authority::Current => Ok(&self.current),
            Authority::Retained(commit) => self
                .retained
                .iter()
                .find(|(retained, _)| *retained == commit)
                .map(|(_, view)| view)
                .ok_or_else(|| anyhow::anyhow!("no retained authority {commit}")),
        }
    }

    fn member_extents(&self, member: &str) -> Result<Vec<(u64, u64)>> {
        self.container
            .member_extents(member)
            .ok_or_else(|| anyhow::anyhow!("container does not hold member {member}"))
    }

    fn part(&self, member: &str) -> Result<&Arc<Part>> {
        self.parts.get(member).ok_or_else(|| anyhow::anyhow!("no manifest names part {member}"))
    }

    fn segment_member(&self, seg: u32) -> String {
        format!("{}/{}", self.fold_prefix, segment::seg_name(seg))
    }

    /// Source byte ranges of the fold frames holding every piece the given locations name.
    fn frame_ranges(
        &self,
        view: &AuthorityView,
        blocks: impl IntoIterator<Item = u32>,
    ) -> Result<Vec<(u64, u64)>> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for block in blocks {
            if !seen.insert(block) {
                continue;
            }
            let (seg, off, len) =
                view.frames.get(block as usize).copied().flatten().ok_or_else(|| {
                    anyhow::anyhow!("block {block} is outside this authority's fold")
                })?;
            let extents = self.member_extents(&self.segment_member(seg))?;
            out.extend(physical_ranges(&extents, off, len));
        }
        Ok(coalesce(out))
    }

    /// The `(offset, length)` ranges of the source that unit `index` reads, coalesced and sorted.
    ///
    /// Metadata the open already read — superblocks, directory, manifests, part footers and
    /// tables of contents, `ids` columns, segment headers and directories — is not repeated
    /// here; a host that opened the verifier holds it. Everything else a unit touches is
    /// declared, and the module's tests hold every unit to that.
    pub fn footprint(&self, index: usize) -> Result<Vec<(u64, u64)>> {
        let unit =
            self.units.get(index).ok_or_else(|| anyhow::anyhow!("no verification unit {index}"))?;
        let ranges = match unit {
            VerifyUnit::MemberChecksum { member, start, stop }
            | VerifyUnit::PartDigest { member, start, stop } => {
                physical_ranges(&self.member_extents(member)?, *start, stop - start)
            }
            VerifyUnit::ManifestChain => {
                let mut out = Vec::new();
                for name in self.container.member_names() {
                    if name == "MANIFEST" || name.starts_with("MANIFEST.") {
                        out.extend(self.member_extents(&name)?);
                    }
                }
                out
            }
            VerifyUnit::PartSection { member, section } => {
                let part = self.part(member)?;
                let (_, off, stored) = part
                    .section_ranges()
                    .into_iter()
                    .find(|(name, _, _)| name == section)
                    .ok_or_else(|| anyhow::anyhow!("part {member} has no section {section}"))?;
                physical_ranges(&self.member_extents(member)?, off, stored)
            }
            VerifyUnit::PartGrammar { member } => self.member_extents(member)?,
            VerifyUnit::PartPieces { member, authority } => {
                let part = self.part(member)?;
                let view = self.authority(*authority)?;
                let mut out =
                    self.part_section_ranges(member, part, |name| name.starts_with("pdict."))?;
                let mut blocks = Vec::new();
                for ordinal in 0..part.piece_count()? {
                    let (location, _) = part.piece(ordinal)?;
                    if !is_punched(view.store.fold.punched_ranges(), location.block_id) {
                        blocks.push(location.block_id);
                    }
                }
                out.extend(self.frame_ranges(view, blocks)?);
                out
            }
            VerifyUnit::FoldFrames { seg, start, stop } => physical_ranges(
                &self.member_extents(&self.segment_member(*seg))?,
                *start,
                stop - start,
            ),
            VerifyUnit::Content { authority, part: index, rows } => {
                let view = self.authority(*authority)?;
                let part = view
                    .store
                    .parts
                    .get(*index)
                    .ok_or_else(|| anyhow::anyhow!("authority has no part {index}"))?;
                let member = &view.store.manifest.parts[*index].member;
                let mut out = self.part_section_ranges(member, part, |name| {
                    name == "cmeta" || name.starts_with("con.") || name.starts_with("pdict.")
                })?;
                let visible = &view.visible[*index];
                let mut blocks = Vec::new();
                for &row in visible.get(rows.clone()).unwrap_or(&[]) {
                    for content in part.contents(row)? {
                        for op in &content.ops {
                            if let ContentOp::Piece { hash, .. } = op {
                                let location = part.find_piece(hash)?.ok_or_else(|| {
                                    anyhow::anyhow!(
                                        "piece {hash} is not in the owning part dictionary"
                                    )
                                })?;
                                if !is_punched(view.store.fold.punched_ranges(), location.block_id)
                                {
                                    blocks.push(location.block_id);
                                }
                            }
                        }
                    }
                }
                out.extend(self.frame_ranges(view, blocks)?);
                out
            }
        };
        Ok(coalesce(ranges))
    }

    fn part_section_ranges(
        &self,
        member: &str,
        part: &Part,
        select: impl Fn(&str) -> bool,
    ) -> Result<Vec<(u64, u64)>> {
        let extents = self.member_extents(member)?;
        let mut out = Vec::new();
        for (name, off, stored) in part.section_ranges() {
            if select(&name) {
                out.extend(physical_ranges(&extents, off, stored));
            }
        }
        Ok(out)
    }

    /// Run unit `index`. Running a unit that already ran is a no-op; a window whose predecessor
    /// has not run is refused; a unit that fails leaves the verifier's state as it was, so the
    /// host can fetch what the failure named and run the same unit again.
    pub fn run(&mut self, index: usize, control: &OperationControl) -> Result<()> {
        if index >= self.units.len() {
            bail!("no verification unit {index}");
        }
        if self.done[index] {
            return Ok(());
        }
        control.check("store verification")?;
        let unit = self.units[index].clone();
        match unit {
            VerifyUnit::MemberChecksum { member, start, stop } => {
                let reader = self
                    .container
                    .extent(&member)
                    .ok_or_else(|| anyhow::anyhow!("container does not hold member {member}"))?;
                let len = self.container.member_len(&member).expect("member exists");
                let expected = self.container.member_checksum(&member).expect("member exists");
                let (next, hasher) = self
                    .member_crc
                    .entry(member.clone())
                    .or_insert_with(|| (0, crc32fast::Hasher::new()));
                if *next != start {
                    bail!("member {member} checksum window at {start} requires the window ending there first");
                }
                let mut working = hasher.clone();
                hash_window(&reader, start, stop, control, |bytes| working.update(bytes))?;
                if stop == len {
                    let got = working.clone().finalize();
                    if got != expected {
                        bail!("container member {member} fails its checksum: {got:08x} != {expected:08x}");
                    }
                    self.member_crc.remove(&member);
                } else {
                    *hasher = working;
                    *next = stop;
                }
            }
            VerifyUnit::ManifestChain => {
                let chain = integrity(
                    "verify retained manifest chain",
                    verify_chain_container_scoped(
                        &self.container,
                        self.read_limits,
                        control,
                        false,
                    ),
                )?;
                self.chain = Some(chain);
            }
            VerifyUnit::PartDigest { member, start, stop } => {
                let part = self.part(&member)?.clone();
                let len = part.byte_len()?;
                let reader = self
                    .container
                    .extent(&member)
                    .ok_or_else(|| anyhow::anyhow!("container does not hold member {member}"))?;
                let (next, hasher) = self
                    .part_blake
                    .entry(member.clone())
                    .or_insert_with(|| (0, blake3::Hasher::new()));
                if *next != start {
                    bail!("part {member} digest window at {start} requires the window ending there first");
                }
                let mut working = hasher.clone();
                hash_window(&reader, start, stop, control, |bytes| {
                    working.update(bytes);
                })?;
                if stop == len {
                    let got = working.finalize().to_hex().to_string();
                    let pins = self.pins.get(&member).cloned().unwrap_or_default();
                    for (commit, want) in &pins {
                        if *want != got {
                            if *commit == 0 {
                                bail!(
                                    "part {member} drifted from the digest the current manifest revision pinned"
                                );
                            }
                            bail!("part {member} drifted from the digest commit {commit} pinned");
                        }
                    }
                    self.part_digests += pins.len();
                    self.part_blake.remove(&member);
                } else {
                    *hasher = working;
                    *next = stop;
                }
            }
            VerifyUnit::PartSection { member, section } => {
                let part = self.part(&member)?.clone();
                integrity(
                    "verify immutable part sections",
                    part.verify_section_with_control(&section, control)
                        .with_context(|| format!("verify section {section} of part {member}")),
                )?;
                if self.current.store.manifest.parts.iter().any(|p| p.member == member) {
                    self.part_sections += 1;
                }
            }
            VerifyUnit::PartGrammar { member } => {
                let part = self.part(&member)?.clone();
                integrity(
                    "verify every physical part row",
                    part.verify_semantics_with_control(control)
                        .with_context(|| format!("verify physical rows of part {member}")),
                )?;
            }
            VerifyUnit::PartPieces { member, authority } => {
                let part = self.part(&member)?.clone();
                let view = self.authority(authority)?;
                integrity(
                    "verify every operational piece dictionary entry",
                    part.verify_piece_dictionary_with_control(&view.store.fold, control)
                        .with_context(|| format!("verify piece dictionary of part {member}")),
                )?;
            }
            VerifyUnit::FoldFrames { seg, start, stop } => {
                let next =
                    self.fold_windows_done.get(&seg).copied().unwrap_or(segment::SEG_HDR_LEN);
                if next != start {
                    bail!("fold segment {seg} window at {start} requires the window ending there first");
                }
                let report = integrity(
                    "verify fold frames",
                    self.current.store.fold.scrub_window_with_control(seg, start, stop, control),
                )?;
                self.fold.blocks += report.blocks;
                self.fold.bytes += report.bytes;
                self.fold.trailing_uncommitted += report.trailing_uncommitted;
                self.fold_windows_done.insert(seg, stop);
                if stop == self.current.store.fold.segment_len(seg)?
                    && self.segments_complete.insert(seg)
                {
                    self.fold.segments += 1;
                }
            }
            VerifyUnit::Content { authority, part: index, rows } => {
                let view = self.authority(authority)?;
                let part = view
                    .store
                    .parts
                    .get(index)
                    .ok_or_else(|| anyhow::anyhow!("authority has no part {index}"))?;
                let visible = &view.visible[index];
                let selected = visible.get(rows).unwrap_or(&[]);
                let mut records = 0usize;
                let mut values = 0usize;
                let mut bytes = 0u64;
                for &row in selected {
                    control.check("store verification")?;
                    let contents = integrity("decode committed record", part.contents(row))?;
                    records += 1;
                    for content in &contents {
                        control.check("store verification")?;
                        let verified = match authority {
                            Authority::Current => integrity(
                                "verify committed content",
                                part.verify_projected_content_with_control(
                                    content,
                                    &view.store.fold,
                                    control,
                                ),
                            )?,
                            Authority::Retained(_) => integrity(
                                "verify retained content",
                                part.verify_retained_projected_content_with_control(
                                    content,
                                    &view.store.fold,
                                    control,
                                ),
                            )?,
                        };
                        values += 1;
                        bytes = bytes.checked_add(verified).ok_or_else(|| {
                            anyhow::anyhow!("verified content byte count overflow")
                        })?;
                    }
                }
                if authority == Authority::Current {
                    self.records += records;
                    self.content_values += values;
                    self.content_bytes = self
                        .content_bytes
                        .checked_add(bytes)
                        .ok_or_else(|| anyhow::anyhow!("verified content byte count overflow"))?;
                    self.content_identities += values;
                }
            }
        }
        self.done[index] = true;
        Ok(())
    }

    /// The composed result, available only once every unit has run. Before that the evidence is
    /// scoped to the units that ran, and this refuses rather than report a whole-store result.
    pub fn report(&self) -> Result<SourceVerification> {
        let (done, total) = self.progress();
        if done != total {
            bail!(
                "verification of {} is scoped to {done} of {total} units; no whole-store result",
                self.label.display()
            );
        }
        let chain = ChainReport {
            part_digests: self.part_digests,
            ..self.chain.expect("the manifest chain unit ran")
        };
        Ok(SourceVerification {
            members: self.container.member_count(),
            member_bytes: self.container.member_bytes(),
            commit: self.current.store.manifest.commit,
            store: StoreVerification {
                chain,
                fold: self.fold,
                parts: self.current.store.parts.len(),
                part_sections: self.part_sections,
                records: self.records,
                content_values: self.content_values,
                content_bytes: self.content_bytes,
                content_identities: self.content_identities,
            },
        })
    }

    /// The store configuration and admission the verifier opened with.
    pub fn fold_cfg(&self) -> FoldCfg {
        self.cfg
    }

    /// The source label the verifier reports under.
    pub fn label(&self) -> &Path {
        &self.label
    }
}

impl AuthorityView {
    fn open(store: ReadStore, control: &OperationControl) -> Result<AuthorityView> {
        control.check("store verification")?;
        let visibility =
            integrity("enumerate committed records", super::read::visibility(&store.parts))?;
        let frames = frame_table(&store.fold)?;
        Ok(AuthorityView { store, visible: visibility.rows, frames })
    }
}

/// `(segment, offset, frame length)` per block id. A frame's length is the distance to the next
/// frame the directory names in the same segment, or to the segment's end for the last one —
/// known without reading a header, which is what lets a footprint be declared before any read.
fn frame_table(fold: &Fold) -> Result<Vec<Option<(u32, u64, u64)>>> {
    let locations = fold.block_locations();
    let mut per_segment: HashMap<u32, Vec<u64>> = HashMap::new();
    for (seg, off) in locations.iter().flatten() {
        per_segment.entry(*seg).or_default().push(u64::from(*off));
    }
    let mut next_after: HashMap<(u32, u64), u64> = HashMap::new();
    for (seg, offsets) in per_segment.iter_mut() {
        offsets.sort_unstable();
        offsets.dedup();
        let len = fold.segment_len(*seg)?;
        for (i, off) in offsets.iter().enumerate() {
            let end = offsets.get(i + 1).copied().unwrap_or(len);
            next_after.insert((*seg, *off), end);
        }
    }
    Ok(locations
        .iter()
        .map(|entry| {
            entry.map(|(seg, off)| {
                let off = u64::from(off);
                let end = next_after[&(seg, off)];
                (seg, off, end - off)
            })
        })
        .collect())
}

fn is_punched(ranges: &[(u32, u32)], block_id: u32) -> bool {
    ranges.iter().any(|&(lo, hi)| (lo..=hi).contains(&block_id))
}

/// `[start, stop)` windows of at most [`WINDOW_BYTES`] over `[from, to)`; an empty range yields
/// exactly one empty window so a zero-length member still has its checksum checked.
fn windows(from: u64, to: u64) -> Vec<(u64, u64)> {
    if to <= from {
        return vec![(from, from)];
    }
    let mut out = Vec::new();
    let mut at = from;
    while at < to {
        let stop = (at + WINDOW_BYTES).min(to);
        out.push((at, stop));
        at = stop;
    }
    out
}

fn row_windows(rows: usize) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < rows {
        let stop = (at + CONTENT_ROWS).min(rows);
        out.push(at..stop);
        at = stop;
    }
    out
}

/// Sort and merge adjacent or overlapping ranges.
fn coalesce(mut ranges: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    ranges.retain(|(_, len)| *len > 0);
    ranges.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (off, len) in ranges {
        match out.last_mut() {
            Some((last_off, last_len)) if off <= *last_off + *last_len => {
                let end = (off + len).max(*last_off + *last_len);
                *last_len = end - *last_off;
            }
            _ => out.push((off, len)),
        }
    }
    out
}

fn hash_window(
    reader: &dyn ReadAt,
    start: u64,
    stop: u64,
    control: &OperationControl,
    mut update: impl FnMut(&[u8]),
) -> Result<()> {
    let mut buf = vec![0u8; (1 << 20).min((stop - start).max(1) as usize)];
    let mut at = start;
    while at < stop {
        control.check("store verification")?;
        let take = buf.len().min((stop - at) as usize);
        reader.read_exact_at(&mut buf[..take], at)?;
        update(&buf[..take]);
        at += take as u64;
    }
    Ok(())
}

fn integrity<T>(context: &'static str, result: Result<T>) -> Result<T> {
    super::verification_integrity(context, result)
}

/// Verify a source-backed container whole: every unit in order, one call. The result matches
/// [`super::verify_container_artifact`] over the same bytes.
pub fn verify_source(
    source: Arc<dyn ReadAt>,
    label: &str,
    cfg: FoldCfg,
    read_limits: ReadLimits,
    control: &OperationControl,
) -> Result<SourceVerification> {
    let mut verifier = SourceVerifier::open(source, label, cfg, read_limits, control)?;
    for index in 0..verifier.units().len() {
        verifier.run(index, control)?;
    }
    verifier.report()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_cover_a_range_exactly_and_an_empty_range_once() {
        assert_eq!(windows(0, 0), vec![(0, 0)]);
        assert_eq!(windows(0, 5), vec![(0, 5)]);
        let big = windows(0, WINDOW_BYTES * 2 + 1);
        assert_eq!(
            big,
            vec![
                (0, WINDOW_BYTES),
                (WINDOW_BYTES, WINDOW_BYTES * 2),
                (WINDOW_BYTES * 2, WINDOW_BYTES * 2 + 1)
            ]
        );
    }

    #[test]
    fn coalesce_merges_touching_and_overlapping_ranges_only() {
        assert_eq!(coalesce(vec![(10, 5), (0, 10), (20, 0), (30, 2)]), vec![(0, 15), (30, 2)]);
        assert_eq!(coalesce(vec![(5, 10), (0, 10)]), vec![(0, 15)]);
    }
}
