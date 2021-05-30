//!
//! This file contains data structures that are explicitly non-portable.
//! They are needed for compatibility with upstream postgres, which does
//! not guarantee portability of data files across CPU architectures.
//!
//! These data structures use native endian and alignment in postgres.
//!
//! This code is not currently guaranteed to work on architectures other
//! than x86_64.
//!
//! Do not use this file, or copy any of the patterns here, for things
//! that are meant to be portable (particularly data structures sent)

use anyhow::{anyhow, Result};
use zerocopy::{AsBytes, FromBytes, LayoutVerified};

pub const PG_CONTROL_FILE_SIZE: usize = 8192;
pub type Oid = u32;
pub type TransactionId = u32;
pub type MultiXactId = TransactionId;
pub type MultiXactOffset = u32;
pub type XLogRecPtr = u64;
pub type TimeLineID = u32;
pub type pg_time_t = i64;
pub type pg_crc32c = u32;

// FIXME: turn this into an enum
pub type DBState = u32;
pub const DBState_DB_STARTUP: DBState = 0;
pub const DBState_DB_SHUTDOWNED: DBState = 1;
pub const DBState_DB_SHUTDOWNED_IN_RECOVERY: DBState = 2;
pub const DBState_DB_SHUTDOWNING: DBState = 3;
pub const DBState_DB_IN_CRASH_RECOVERY: DBState = 4;
pub const DBState_DB_IN_ARCHIVE_RECOVERY: DBState = 5;
pub const DBState_DB_IN_PRODUCTION: DBState = 6;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, AsBytes, FromBytes)]
pub struct FullTransactionId {
    pub value: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Default, AsBytes, FromBytes)]
pub struct CheckPoint {
    pub redo: XLogRecPtr,
    pub ThisTimeLineID: TimeLineID,
    pub PrevTimeLineID: TimeLineID,
    /// Note this is `bool` in C; it is u8 to allow safe conversions.
    pub fullPageWrites: u8,
    /// Explicit padding to align the 64-bit field that follows.
    pub __padding1: [u8; 7],
    pub nextXid: FullTransactionId,
    pub nextOid: Oid,
    pub nextMulti: MultiXactId,
    pub nextMultiOffset: MultiXactOffset,
    pub oldestXid: TransactionId,
    pub oldestXidDB: Oid,
    pub oldestMulti: MultiXactId,
    pub oldestMultiDB: Oid,
    /// Explicit padding to align the 64-bit field that follows.
    pub __padding4: [u8; 4],
    pub time: pg_time_t,
    pub oldestCommitTsXid: TransactionId,
    pub newestCommitTsXid: TransactionId,
    pub oldestActiveXid: TransactionId,
    /// Explicit padding to align the end of the struct, so this
    /// struct can be included inside other structs.
    pub __padding5: [u8; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Default, AsBytes, FromBytes)]
pub struct ControlFileData {
    pub system_identifier: u64,
    pub pg_control_version: u32,
    pub catalog_version_no: u32,
    pub state: DBState,
    /// Explicit padding to align the 64-bit field that follows.
    pub __padding1: [u8; 4],
    pub time: pg_time_t,
    pub checkPoint: XLogRecPtr,
    pub checkPointCopy: CheckPoint,
    pub unloggedLSN: XLogRecPtr,
    pub minRecoveryPoint: XLogRecPtr,
    pub minRecoveryPointTLI: TimeLineID,
    /// Explicit padding to align the 64-bit field that follows.
    pub __padding2: [u8; 4],
    pub backupStartPoint: XLogRecPtr,
    pub backupEndPoint: XLogRecPtr,
    /// Note this is `bool` in C; it is u8 to allow safe conversions.
    pub backupEndRequired: u8,
    /// Explicit padding to align the 32-bit field that follows.
    pub __padding3: [u8; 3],
    pub wal_level: u32,
    /// Note this is `bool` in C; it is u8 to allow safe conversions.
    pub wal_log_hints: u8,
    /// Explicit padding to align the 32-bit field that follows.
    pub __padding4: [u8; 3],
    pub MaxConnections: u32,
    pub max_worker_processes: u32,
    pub max_wal_senders: u32,
    pub max_prepared_xacts: u32,
    pub max_locks_per_xact: u32,
    /// Note this is `bool` in C; it is u8 to allow safe conversions.
    pub track_commit_timestamp: u8,
    /// Explicit padding to align the 32-bit field that follows.
    pub __padding5: [u8; 3],
    pub maxAlign: u32,
    pub floatFormat: f64,
    pub blcksz: u32,
    pub relseg_size: u32,
    pub xlog_blcksz: u32,
    pub xlog_seg_size: u32,
    pub nameDataLen: u32,
    pub indexMaxKeys: u32,
    pub toast_max_chunk_size: u32,
    pub loblksize: u32,
    // /// Note this is `bool` in C; it is u8 to allow safe conversions.
    pub float8ByVal: u8,
    /// Explicit padding to align the 32-bit field that follows.
    pub __padding6: [u8; 3],
    pub data_checksum_version: u32,
    pub mock_authentication_nonce: [u8; 32],
    pub crc: pg_crc32c,
    /// Explicit padding to align the end of the struct, to satisfy `zerocopy`
    pub __padding7: [u8; 4],
}

impl ControlFileData {
    // FIXME: compute this in a better way, or remove it entirely?
    const OFFSETOF_CRC: usize = 288;

    /// Decode a `ControlFileData` struct from a byte array.
    ///
    /// This action is non-portable; it may fail to read data written by other
    /// CPU architectures.
    ///
    pub fn decode(buf: &[u8]) -> Result<ControlFileData> {
        // Verify correct buffer alignment and size.
        let (layout, _remaining) = LayoutVerified::<_, ControlFileData>::new_from_prefix(buf)
            .ok_or(anyhow!("failed to get LayoutVerified ref"))?;

        // Safely transmute into &ControlFileData, and then clone to get an owned copy.
        let controlfile = layout.into_ref().clone();

        // Compute expected CRC.
        // Note the buffer length was already checked by LayoutVerified, so
        // accessing this offset should never panic.
        let data_without_crc = &buf[0..Self::OFFSETOF_CRC];
        let expectedcrc = crc32c::crc32c(&data_without_crc);

        if expectedcrc != controlfile.crc {
            anyhow::bail!(
                "invalid CRC in control file: expected {:08X}, was {:08X}",
                expectedcrc,
                controlfile.crc
            );
        }

        Ok(controlfile)
    }

    /// Encode a `ControlFileData` struct into a byte array.
    ///
    /// This action is non-portable; it may fail to read data written by other
    /// CPU architectures.
    ///
    pub fn encode(mut self) -> Box<[u8]> {
        let cf_bytes = self.as_bytes();

        // Recompute the CRC
        let data_without_crc = &cf_bytes[0..Self::OFFSETOF_CRC];
        let newcrc = crc32c::crc32c(&data_without_crc);

        // Drop the immutable reference so we can modify the struct
        drop(cf_bytes);
        self.crc = newcrc;

        // Reacquire the reference so we can produce the output bytes
        let cf_bytes = self.as_bytes();
        cf_bytes.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindgen_bindings;
    use std::mem::size_of;

    #[test]
    fn test_struct_compatibility() {
        // This test could be a lot more exhaustive. It could, for example, fill in
        // every field with a unique value and then verify that the "mirror" struct
        // sees the same values in the same locations. That seems like a lot of work,
        // probably better handled by a proc-macro of some kind.

        assert_eq!(
            size_of::<ControlFileData>(),
            size_of::<bindgen_bindings::ControlFileData>()
        );

        // Do a spot check by assigning the last field.
        let mut cfd = ControlFileData::default();
        cfd.crc = 0x12345678;

        // A transmute from struct-with-explicit-padding to struct-with-implicit-padding
        // should be well-defined behavior; the other way around is probably UB.
        let cfd_bindgen: &bindgen_bindings::ControlFileData = unsafe { std::mem::transmute(&cfd) };
        assert_eq!(cfd_bindgen.crc, 0x12345678);
    }
}
