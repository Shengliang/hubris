// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! API crate for the Gimlet Host Flash server.

#![no_std]

use drv_hash_api::SHA256_SZ;
use userlib::*;
use zerocopy::AsBytes;

/// Errors that can be produced from the host flash server API.
///
/// This enumeration doesn't include errors that result from configuration
/// issues, like sending host flash messages to some other task.
#[derive(Copy, Clone, Debug, FromPrimitive, PartialEq)]
pub enum HfError {
    WriteEnableFailed = 1,
    ServerRestarted = 2,
    MuxFailed = 3,
    HashBadRange = 4,
    HashError = 5,
    HashNotConfigured = 6,
}

impl From<HfError> for u16 {
    fn from(rc: HfError) -> Self {
        rc as u16
    }
}

impl From<HfError> for u32 {
    fn from(rc: HfError) -> Self {
        rc as u32
    }
}

impl core::convert::TryFrom<u32> for HfError {
    type Error = ();
    fn try_from(rc: u32) -> Result<Self, Self::Error> {
        Self::from_u32(rc).ok_or(())
    }
}

#[derive(Copy, Clone, Debug, FromPrimitive, PartialEq, AsBytes)]
#[repr(u8)]
pub enum HfMuxState {
    SP = 1,
    HostCPU = 2,
}

include!(concat!(env!("OUT_DIR"), "/client_stub.rs"));
