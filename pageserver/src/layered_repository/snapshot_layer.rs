//!
//! A SnapshotLayer represents one snapshot file on disk. One file holds all page versions
//! and size information of one relation, in a range of LSN.
//! The name "snapshot file" is a bit of a misnomer because a snapshot file doesn't
//! contain a snapshot at a specific LSN, but rather all the page versions in a range
//! of LSNs.
//!
//! Currently, a snapshot file contains full information needed to reconstruct any
//! page version in the LSN range, without consulting any other snapshot files. When
//! a new snapshot file is created for writing, the full contents of relation are
//! materialized as it is at the beginning of the LSN range. That can be very expensive,
//! we should find a way to store differential files. But this keeps the read-side
//! of things simple. You can find the correct snapshot file based on RelTag and
//! timeline+LSN, and once you've located it, you have all the data you need to in that
//! file.
//!
//! When a snapshot file needs to be accessed, we slurp the whole file into memory, into
//! a SnapshotLayer struct.
//!
//! On disk, a snapshot file is actually two files: one containing all the page versions,
//! and another containing the relation size information. That's just for the convenience
//! of serializing the two objects.
//!
//! The files are stored in .zenith/timelines/<timelineid> directory.
//! Currently, there are no subdirectories, and each snapshot file is named like this:
//!
//!    <spcnode>_<dbnode>_<relnode>_<forknum>_<start LSN>_<end LSN>
//!
//! And the corresponding file containing the relation size information has _relsizes
//! suffix. For example:
//!
//!    1663_13990_2609_0_000000000169C348_000000000169C349
//!    1663_13990_2609_0_000000000169C348_000000000169C349_relsizes
//!

use crate::layered_repository::storage_layer::Layer;
use crate::layered_repository::storage_layer::PageVersion;
use crate::repository::{RelTag, WALRecord};
use crate::walredo::WalRedoManager;
use crate::PageServerConf;
use crate::ZTimelineId;
use anyhow::{bail, Result};
use bytes::Bytes;
use log::*;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::fs::File;
use std::io::Write;
use std::ops::Bound::Included;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use zenith_utils::bin_ser::BeSer;
use zenith_utils::lsn::Lsn;

static ZERO_PAGE: Bytes = Bytes::from_static(&[0u8; 8192]);

///
/// SnapshotLayer is the in-memory data structure associated with an on-disk snapshot file.
/// It is also used to accumulate new changes at the tip of a branch; end_lsn is u64::MAX
/// in that case.
///
pub struct SnapshotLayer {
    conf: &'static PageServerConf,
    pub timelineid: ZTimelineId,
    pub tag: RelTag,

    //
    // This entry contains all the changes from 'start_lsn' to 'end_lsn'. The
    // start is inclusive, and end is exclusive.
    pub start_lsn: Lsn,
    pub end_lsn: Lsn,

    ///
    /// All versions of all pages in the file are are kept here.
    /// Indexed by block number and LSN.
    ///
    page_versions: Mutex<BTreeMap<(u32, Lsn), PageVersion>>,

    ///
    /// `relsizes` tracks the size of the relation at different points in time.
    ///
    relsizes: Mutex<BTreeMap<Lsn, u32>>,
}

impl Layer for SnapshotLayer {
    fn is_frozen(&self) -> bool {
        return true;
    }

    fn get_timeline_id(&self) -> ZTimelineId {
        return self.timelineid;
    }

    fn get_tag(&self) -> RelTag {
        return self.tag;
    }

    fn get_start_lsn(&self) -> Lsn {
        return self.start_lsn;
    }

    fn get_end_lsn(&self) -> Lsn {
        return self.end_lsn;
    }

    /// Look up given page in the cache.
    fn get_page_at_lsn(
        &self,
        walredo_mgr: &dyn WalRedoManager,
        blknum: u32,
        lsn: Lsn,
    ) -> Result<Bytes> {
        // Scan the BTreeMap backwards, starting from the given entry.
        let mut records: Vec<WALRecord> = Vec::new();
        let mut page_img: Option<Bytes> = None;
        let mut need_base_image_lsn: Option<Lsn> = Some(lsn);
        {
            let page_versions = self.page_versions.lock().unwrap();
            let minkey = (blknum, Lsn(0));
            let maxkey = (blknum, lsn);
            let mut iter = page_versions.range((Included(&minkey), Included(&maxkey)));
            while let Some(((_blknum, entry_lsn), entry)) = iter.next_back() {
                if let Some(img) = &entry.page_image {
                    page_img = Some(img.clone());
                    need_base_image_lsn = None;
                    break;
                } else if let Some(rec) = &entry.record {
                    records.push(rec.clone());
                    if rec.will_init {
                        // This WAL record initializes the page, so no need to go further back
                        need_base_image_lsn = None;
                        break;
                    } else {
                        need_base_image_lsn = Some(*entry_lsn);
                    }
                } else {
                    // No base image, and no WAL record. Huh?
                    bail!("no page image or WAL record for requested page");
                }
            }

            // release lock on 'page_versions'
        }
        records.reverse();

        // If we needed a base image to apply the WAL records against, we should have found it in memory.
        if let Some(lsn) = need_base_image_lsn {
            if records.is_empty() {
                // no records, and no base image. This can happen if PostgreSQL extends a relation
                // but never writes the page.
                //
                // Would be nice to detect that situation better.
                warn!("Page {:?}/{} at {} not found", self.tag, blknum, lsn);
                return Ok(ZERO_PAGE.clone());
            }
            bail!(
                "No base image found for page {} blk {} at {}/{}",
                self.tag,
                blknum,
                self.timelineid,
                lsn
            );
        }

        // If we have a page image, and no WAL, we're all set
        if records.is_empty() {
            if let Some(img) = page_img {
                trace!(
                    "found page image for blk {} in {} at {}/{}, no WAL redo required",
                    blknum,
                    self.tag,
                    self.timelineid,
                    lsn
                );
                Ok(img)
            } else {
                // FIXME: this ought to be an error?
                warn!("Page {:?}/{} at {} not found", self.tag, blknum, lsn);
                Ok(ZERO_PAGE.clone())
            }
        } else {
            // We need to do WAL redo.
            //
            // If we don't have a base image, then the oldest WAL record better initialize
            // the page
            if page_img.is_none() && !records.first().unwrap().will_init {
                // FIXME: this ought to be an error?
                warn!(
                    "Base image for page {:?}/{} at {} not found, but got {} WAL records",
                    self.tag,
                    blknum,
                    lsn,
                    records.len()
                );
                Ok(ZERO_PAGE.clone())
            } else {
                if page_img.is_some() {
                    trace!("found {} WAL records and a base image for blk {} in {} at {}/{}, performing WAL redo", records.len(), blknum, self.tag, self.timelineid, lsn);
                } else {
                    trace!("found {} WAL records that will init the page for blk {} in {} at {}/{}, performing WAL redo", records.len(), blknum, self.tag, self.timelineid, lsn);
                }
                let img = walredo_mgr.request_redo(
                    self.tag,
                    blknum,
                    lsn,
                    page_img,
                    records,
                )?;

                // FIXME: Should we memoize the page image in memory, so that
                // we wouldn't need to reconstruct it again, if it's requested again?
                //self.put_page_image(blknum, lsn, img.clone())?;

                Ok(img)
            }
        }
    }

    /// Get size of the relation at given LSN
    fn get_rel_size(&self, lsn: Lsn) -> Result<u32> {
        // Scan the BTreeMap backwards, starting from the given entry.
        let relsizes = self.relsizes.lock().unwrap();
        let mut iter = relsizes.range((Included(&Lsn(0)), Included(&lsn)));

        if let Some((_entry_lsn, entry)) = iter.next_back() {
            trace!("get_relsize: {} at {} -> {}", self.tag, lsn, *entry);
            Ok(*entry)
        } else {
            bail!(
                "No size found for relfile {:?} at {} in memory",
                self.tag,
                lsn
            );
        }
    }

    /// Does this relation exist at given LSN?
    fn get_rel_exists(&self, lsn: Lsn) -> Result<bool> {
        // Scan the BTreeMap backwards, starting from the given entry.
        let relsizes = self.relsizes.lock().unwrap();

        let mut iter = relsizes.range((Included(&Lsn(0)), Included(&lsn)));

        let result = if let Some((_entry_lsn, _entry)) = iter.next_back() {
            true
        } else {
            false
        };
        Ok(result)
    }

    // Unsupported write operations
    fn put_page_version(&self, blknum: u32, lsn: Lsn, _pv: PageVersion) -> Result<()> {
        panic!(
            "cannot modify historical snapshot file, rel {} blk {} at {}/{}, {}-{}",
            self.tag, blknum, self.timelineid, lsn, self.start_lsn, self.end_lsn
        );
    }
    fn put_truncation(&self, _lsn: Lsn, _relsize: u32) -> anyhow::Result<()> {
        bail!("cannot modify historical snapshot file");
    }

    fn freeze(&self, _end_lsn: Lsn) -> Result<()> {
        bail!("cannot freeze historical snapshot file");
    }
}

impl SnapshotLayer {
    fn path(&self) -> PathBuf {
        Self::path_for(
            self.conf,
            self.timelineid,
            self.tag,
            self.start_lsn,
            self.end_lsn,
        )
    }

    fn path_for(
        conf: &'static PageServerConf,
        timelineid: ZTimelineId,
        tag: RelTag,
        start_lsn: Lsn,
        end_lsn: Lsn,
    ) -> PathBuf {
        let fname = format!(
            "{}_{}_{}_{}_{:016X}_{:016X}",
            tag.spcnode,
            tag.dbnode,
            tag.relnode,
            tag.forknum,
            u64::from(start_lsn),
            u64::from(end_lsn)
        );

        conf.timeline_path(timelineid).join(&fname)
    }

    fn relsizes_path(path: &Path) -> PathBuf {
        let mut fname = path.file_name().unwrap().to_os_string();
        fname.push("_relsizes");

        path.with_file_name(fname)
    }

    /// Create a new snapshot file, using the given btreemaps containing the page versions and
    /// relsizes.
    ///
    /// This is used to write the in-memory layer to disk. The in-memory layer uses the same
    /// data structure with two btreemaps as we do, so passing the btreemaps is currently
    /// expedient.
    pub fn create(
        conf: &'static PageServerConf,
        timelineid: ZTimelineId,
        tag: RelTag,
        start_lsn: Lsn,
        end_lsn: Lsn,
        page_versions: BTreeMap<(u32, Lsn), PageVersion>,
        relsizes: BTreeMap<Lsn, u32>,
    ) -> Result<SnapshotLayer> {
        let snapfile = SnapshotLayer {
            conf: conf,
            timelineid: timelineid,
            tag: tag,
            start_lsn: start_lsn,
            end_lsn,
            page_versions: Mutex::new(page_versions),
            relsizes: Mutex::new(relsizes),
        };

        snapfile.save()?;
        Ok(snapfile)
    }

    /// Write the in-memory btreemaps into files
    fn save(&self) -> Result<()> {
        let path = self.path();

        let page_versions = self.page_versions.lock().unwrap();
        let relsizes = self.relsizes.lock().unwrap();

        // Note: This overwrites any existing file. There shouldn't be any.
        // FIXME: throw an error instead?

        // Write out page versions
        let mut file = File::create(&path)?;
        let buf = BTreeMap::ser(&page_versions)?;
        file.write_all(&buf)?;

        // and relsizes to separate file
        let mut file = File::create(Self::relsizes_path(&path))?;
        let buf = BTreeMap::ser(&relsizes)?;
        file.write_all(&buf)?;

        debug!("saved {}", &path.display());

        Ok(())
    }

    ///
    /// Find the snapshot file with latest LSN that covers the given 'lsn', or is before it.
    ///
    pub fn find_latest_snapshot_file(
        conf: &'static PageServerConf,
        timelineid: ZTimelineId,
        tag: RelTag,
        lsn: Lsn,
    ) -> Result<Option<(Lsn, Lsn)>> {
        // Scan the timeline directory to get all rels in this timeline.
        let path = conf.timeline_path(timelineid);
        let mut result_start_lsn = Lsn(0);
        let mut result_end_lsn = Lsn(0);
        for direntry in fs::read_dir(path)? {
            let direntry = direntry?;

            let fname = direntry.file_name();
            let fname = fname.to_str().unwrap();

            if let Some((reltag, start_lsn, end_lsn)) = Self::fname_to_tag(fname) {
                if reltag == tag && start_lsn <= lsn && start_lsn > result_start_lsn {
                    result_start_lsn = start_lsn;
                    result_end_lsn = end_lsn;
                }
            }
        }
        if result_start_lsn != Lsn(0) {
            Ok(Some((result_start_lsn, result_end_lsn)))
        } else {
            Ok(None)
        }
    }

    ///
    /// Load the state for one relation back into memory.
    ///
    /// Returns the latest snapshot file that before the given 'lsn'.
    ///
    pub fn load(
        conf: &'static PageServerConf,
        timelineid: ZTimelineId,
        tag: RelTag,
        lsn: Lsn,
    ) -> Result<Option<SnapshotLayer>> {
        if let Some((start_lsn, end_lsn)) =
            Self::find_latest_snapshot_file(conf, timelineid, tag, lsn)?
        {
            let snap = Self::load_path(conf, timelineid, tag, start_lsn, end_lsn)?;
            Ok(Some(snap))
        } else {
            Ok(None)
        }
    }

    fn load_path(
        conf: &'static PageServerConf,
        timelineid: ZTimelineId,
        tag: RelTag,
        start_lsn: Lsn,
        end_lsn: Lsn,
    ) -> Result<SnapshotLayer> {
        let path = Self::path_for(conf, timelineid, tag, start_lsn, end_lsn);

        let content = std::fs::read(&path)?;
        let page_versions = BTreeMap::des(&content)?;
        debug!("loaded from {}", &path.display());

        let content = std::fs::read(Self::relsizes_path(&path))?;
        let relsizes = BTreeMap::des(&content)?;
        Ok(SnapshotLayer {
            conf,
            timelineid,
            tag,
            start_lsn,
            end_lsn,
            page_versions: Mutex::new(page_versions),
            relsizes: Mutex::new(relsizes),
        })
    }

    pub fn list_rels(
        conf: &'static PageServerConf,
        timelineid: ZTimelineId,
        spcnode: u32,
        dbnode: u32,
    ) -> Result<HashSet<RelTag>> {
        let mut rels: HashSet<RelTag> = HashSet::new();

        // Scan the timeline directory to get all rels in this timeline.
        let path = conf.timeline_path(timelineid);
        for direntry in fs::read_dir(path)? {
            let direntry = direntry?;

            let fname = direntry.file_name();
            let fname = fname.to_str().unwrap();

            if let Some((reltag, _start_lsn, _end_lsn)) = Self::fname_to_tag(fname) {
                if (spcnode == 0 || reltag.spcnode == spcnode)
                    && (dbnode == 0 || reltag.dbnode == dbnode)
                {
                    rels.insert(reltag);
                }
            }
        }
        Ok(rels)
    }

    fn fname_to_tag(fname: &str) -> Option<(RelTag, Lsn, Lsn)> {
        // Split the filename into parts
        //
        //    <spcnode>_<dbnode>_<relnode>_<forknum>_<start LSN>_<end LSN>
        //
        let mut parts = fname.split('_');

        let reltag = RelTag {
            spcnode: parts.next()?.parse::<u32>().ok()?,
            dbnode: parts.next()?.parse::<u32>().ok()?,
            relnode: parts.next()?.parse::<u32>().ok()?,
            forknum: parts.next()?.parse::<u8>().ok()?,
        };
        let start_lsn = Lsn::from_hex(parts.next()?).ok()?;
        let end_lsn = Lsn::from_hex(parts.next()?).ok()?;

        Some((reltag, start_lsn, end_lsn))
    }
}