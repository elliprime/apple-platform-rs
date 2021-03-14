// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/*! Mach-O primitives related to code signing

There is no official specification of the Mach-O structure for various
code signing primitives. So the definitions in here could diverge from
what is actually implemented.

The best source of the specification comes from Apple's open source headers,
notably cs_blobs.h (e.g.
https://opensource.apple.com/source/xnu/xnu-7195.81.3/osfmk/kern/cs_blobs.h.auto.html).
(Go to https://opensource.apple.com/source/xnu and check for newer versions of xnu
to look for new features.)

Code signing data is embedded within the named `__LINKEDIT` segment of
the Mach-O binary. An `LC_CODE_SIGNATURE` load command in the Mach-O header
will point you at this data. See `find_signature_data()` for this logic.

Within the `__LINKEDIT` segment we have a number of data structures
describing code signing information. The high-level format of these
data structures within the segment is roughly as follows:

* A `SuperBlob` header describes the total length of data and the number of
  *blob* sections that follow.
* An array of `BlobIndex` describing the type and offset of all *blob* sections
  that follow. The *type* here is a *slot* and describes what type of data the
  *blob* contains (code directory, entitlements, embedded signature, etc).
* N *blob* sections of varying formats and lengths.

We only support the `CSMAGIC_EMBEDDED_SIGNATURE` magic in the `SuperBlob`, as
this is what is used in the wild. (It is even unclear if other `CSMAGIC_*`
values can occur in `SuperBlob` headers.)

The `EmbeddedSignature` type represents a lightly parsed `SuperBlob`. It
provides access to `BlobEntry` which describe the *blob* sections within the
super blob. A `BlobEntry` can be parsed into the more concrete `ParsedBlob`,
which allows some access to data within each specific blob type.
*/

use {
    goblin::mach::{constants::SEG_LINKEDIT, load_command::CommandVariant, MachO},
    scroll::Pread,
    std::{
        collections::HashMap,
        convert::{TryFrom, TryInto},
    },
};

// Constants identifying payload of Blob entries.
const CSSLOT_CODEDIRECTORY: u32 = 0;
const CSSLOT_INFOSLOT: u32 = 1;
const CSSLOT_REQUIREMENTS: u32 = 2;
const CSSLOT_RESOURCEDIR: u32 = 3;
const CSSLOT_APPLICATION: u32 = 4;
const CSSLOT_ENTITLEMENTS: u32 = 5;

/// First alternate CodeDirectory, if any
const CSSLOT_ALTERNATE_CODEDIRECTORY_0: u32 = 0x1000;
const CSSLOT_ALTERNATE_CODEDIRECTORY_1: u32 = 0x1001;
const CSSLOT_ALTERNATE_CODEDIRECTORY_2: u32 = 0x1002;
const CSSLOT_ALTERNATE_CODEDIRECTORY_3: u32 = 0x1003;
const CSSLOT_ALTERNATE_CODEDIRECTORY_4: u32 = 0x1004;

/// CMS signature.
const CSSLOT_SIGNATURESLOT: u32 = 0x10000;
const CSSLOT_IDENTIFICATIONSLOT: u32 = 0x10001;
const CSSLOT_TICKETSLOT: u32 = 0x10002;

/// Defines a typed slot within code signing data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CodeSigningSlot {
    CodeDirectory,
    Info,
    Requirements,
    ResourceDir,
    Application,
    Entitlements,
    AlternateCodeDirectory0,
    AlternateCodeDirectory1,
    AlternateCodeDirectory2,
    AlternateCodeDirectory3,
    AlternateCodeDirectory4,
    Signature,
    Identification,
    Ticket,
    Unknown(u32),
}

impl From<u32> for CodeSigningSlot {
    fn from(v: u32) -> Self {
        match v {
            CSSLOT_CODEDIRECTORY => Self::CodeDirectory,
            CSSLOT_INFOSLOT => Self::Info,
            CSSLOT_REQUIREMENTS => Self::Requirements,
            CSSLOT_RESOURCEDIR => Self::ResourceDir,
            CSSLOT_APPLICATION => Self::Application,
            CSSLOT_ENTITLEMENTS => Self::Entitlements,
            CSSLOT_ALTERNATE_CODEDIRECTORY_0 => Self::AlternateCodeDirectory0,
            CSSLOT_ALTERNATE_CODEDIRECTORY_1 => Self::AlternateCodeDirectory1,
            CSSLOT_ALTERNATE_CODEDIRECTORY_2 => Self::AlternateCodeDirectory2,
            CSSLOT_ALTERNATE_CODEDIRECTORY_3 => Self::AlternateCodeDirectory3,
            CSSLOT_ALTERNATE_CODEDIRECTORY_4 => Self::AlternateCodeDirectory4,
            CSSLOT_SIGNATURESLOT => Self::Signature,
            CSSLOT_IDENTIFICATIONSLOT => Self::Identification,
            CSSLOT_TICKETSLOT => Self::Ticket,
            _ => Self::Unknown(v),
        }
    }
}

impl Into<u32> for CodeSigningSlot {
    fn into(self) -> u32 {
        match self {
            Self::CodeDirectory => CSSLOT_CODEDIRECTORY,
            Self::Info => CSSLOT_INFOSLOT,
            Self::Requirements => CSSLOT_REQUIREMENTS,
            Self::ResourceDir => CSSLOT_RESOURCEDIR,
            Self::Application => CSSLOT_APPLICATION,
            Self::Entitlements => CSSLOT_ENTITLEMENTS,
            Self::AlternateCodeDirectory0 => CSSLOT_ALTERNATE_CODEDIRECTORY_0,
            Self::AlternateCodeDirectory1 => CSSLOT_ALTERNATE_CODEDIRECTORY_1,
            Self::AlternateCodeDirectory2 => CSSLOT_ALTERNATE_CODEDIRECTORY_2,
            Self::AlternateCodeDirectory3 => CSSLOT_ALTERNATE_CODEDIRECTORY_3,
            Self::AlternateCodeDirectory4 => CSSLOT_ALTERNATE_CODEDIRECTORY_4,
            Self::Signature => CSSLOT_SIGNATURESLOT,
            Self::Identification => CSSLOT_IDENTIFICATIONSLOT,
            Self::Ticket => CSSLOT_TICKETSLOT,
            Self::Unknown(v) => v,
        }
    }
}

/// Single requirement blob.
const CSMAGIC_REQUIREMENT: u32 = 0xfade0c00;

/// Requirements vector (internal requirements).
const CSMAGIC_REQUIREMENTS: u32 = 0xfade0c01;

/// CodeDirectory blob.
const CSMAGIC_CODEDIRECTORY: u32 = 0xfade0c02;

/// Embedded form of signature data.
const CSMAGIC_EMBEDDED_SIGNATURE: u32 = 0xfade0cc0;

/// XXX
const CSMAGIC_EMBEDDED_SIGNATURE_OLD: u32 = 0xfade0b02;

/// Embedded entitlements.
const CSMAGIC_EMBEDDED_ENTITLEMENTS: u32 = 0xfade7171;

/// Multi-arch collection of embedded signatures.
const CSMAGIC_DETACHED_SIGNATURE: u32 = 0xfade0cc1;

/// CMS signature, among other things.
const CSMAGIC_BLOBWRAPPER: u32 = 0xfade0b01;

/// Defines header magic for various payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CodeSigningMagic {
    Requirement,
    Requirements,
    CodeDirectory,
    EmbeddedSignature,
    EmbeddedSignatureOld,
    EmbeddedEntitlements,
    DetachedSignature,
    BlobWrapper,
    Unknown(u32),
}

impl From<u32> for CodeSigningMagic {
    fn from(v: u32) -> Self {
        match v {
            CSMAGIC_REQUIREMENT => Self::Requirement,
            CSMAGIC_REQUIREMENTS => Self::Requirements,
            CSMAGIC_CODEDIRECTORY => Self::CodeDirectory,
            CSMAGIC_EMBEDDED_SIGNATURE => Self::EmbeddedSignature,
            CSMAGIC_EMBEDDED_SIGNATURE_OLD => Self::EmbeddedSignatureOld,
            CSMAGIC_EMBEDDED_ENTITLEMENTS => Self::EmbeddedEntitlements,
            CSMAGIC_DETACHED_SIGNATURE => Self::DetachedSignature,
            CSMAGIC_BLOBWRAPPER => Self::BlobWrapper,
            _ => Self::Unknown(v),
        }
    }
}

impl Into<u32> for CodeSigningMagic {
    fn into(self) -> u32 {
        match self {
            Self::Requirement => CSMAGIC_REQUIREMENT,
            Self::Requirements => CSMAGIC_REQUIREMENTS,
            Self::CodeDirectory => CSMAGIC_CODEDIRECTORY,
            Self::EmbeddedSignature => CSMAGIC_EMBEDDED_SIGNATURE,
            Self::EmbeddedSignatureOld => CSMAGIC_EMBEDDED_SIGNATURE_OLD,
            Self::EmbeddedEntitlements => CSMAGIC_EMBEDDED_ENTITLEMENTS,
            Self::DetachedSignature => CSMAGIC_DETACHED_SIGNATURE,
            Self::BlobWrapper => CSMAGIC_BLOBWRAPPER,
            Self::Unknown(v) => v,
        }
    }
}

// Executable segment flags.

/// Executable segment denotes main binary.
pub const CS_EXECSEG_MAIN_BINARY: u32 = 0x1;

/// Allow unsigned pages (for debugging)
pub const CS_EXECSEG_ALLOW_UNSIGNED: u32 = 0x10;

/// Main binary is debugger.
pub const CS_EXECSEG_DEBUGGER: u32 = 0x20;

/// JIT enabled.
pub const CS_EXECSEG_JIT: u32 = 0x40;

/// Obsolete: skip library validation.
pub const CS_EXECSEG_SKIP_LV: u32 = 0x80;

/// Can bless cdhash for execution.
pub const CS_EXECSEG_CAN_LOAD_CDHASH: u32 = 0x100;

/// Can execute blessed cdhash.
pub const CS_EXECSEG_CAN_EXEC_CDHASH: u32 = 0x200;

// Versions of CodeDirectory struct.
const CS_SUPPORTSSCATTER: u32 = 0x20100;
const CS_SUPPORTSTEAMID: u32 = 0x20200;
const CS_SUPPORTSCODELIMIT64: u32 = 0x20300;
const CS_SUPPORTSEXECSEG: u32 = 0x20400;
const CS_SUPPORTSRUNTIME: u32 = 0x20500;
const CS_SUPPORTSLINKAGE: u32 = 0x20600;

/// Compat with amfi
pub const CSTYPE_INDEX_REQUIREMENTS: u32 = 0x00000002;
pub const CSTYPE_INDEX_ENTITLEMENTS: u32 = 0x00000005;

const CS_HASHTYPE_SHA1: u8 = 1;
const CS_HASHTYPE_SHA256: u8 = 2;
const CS_HASHTYPE_SHA256_TRUNCATED: u8 = 3;
const CS_HASHTYPE_SHA384: u8 = 4;

pub const CS_SHA1_LEN: u32 = 20;
pub const CS_SHA256_LEN: u32 = 32;
pub const CS_SHA256_TRUNCATED_LEN: u32 = 20;

/// always - larger hashes are truncated
pub const CS_CDHASH_LEN: u32 = 20;
/// max size of the hash we'll support
pub const CS_HASH_MAX_SIZE: u32 = 48;

/*
 * Currently only to support Legacy VPN plugins, and Mac App Store
 * but intended to replace all the various platform code, dev code etc. bits.
 */
pub const CS_SIGNER_TYPE_UNKNOWN: u32 = 0;
pub const CS_SIGNER_TYPE_LEGACYVPN: u32 = 5;
pub const CS_SIGNER_TYPE_MAC_APP_STORE: u32 = 6;

pub const CS_SUPPL_SIGNER_TYPE_UNKNOWN: u32 = 0;
pub const CS_SUPPL_SIGNER_TYPE_TRUSTCACHE: u32 = 7;
pub const CS_SUPPL_SIGNER_TYPE_LOCAL: u32 = 8;

#[repr(C)]
#[derive(Clone, Pread)]
struct BlobIndex {
    /// Corresponds to a CSSLOT_* constant.
    typ: u32,
    offset: u32,
}

/// Read the header from a Blob.
///
/// Blobs begin with a u32 magic and u32 length, inclusive.
fn read_blob_header(data: &[u8]) -> Result<(u32, usize, &[u8]), scroll::Error> {
    let magic = data.pread_with(0, scroll::BE)?;
    let length = data.pread_with::<u32>(4, scroll::BE)?;

    Ok((magic, length as usize, &data[8..]))
}

fn read_and_validate_blob_header(
    data: &[u8],
    expected_magic: u32,
) -> Result<&[u8], MachOParseError> {
    let (magic, _, data) = read_blob_header(data)?;

    if magic != expected_magic {
        Err(MachOParseError::BadMagic)
    } else {
        Ok(data)
    }
}

impl std::fmt::Debug for BlobIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("BlobIndex")
            .field("type", &CodeSigningSlot::from(self.typ))
            .field("offset", &self.offset)
            .finish()
    }
}

/// Represents embedded signature data in a Mach-O binary.
///
/// This type represents a lightly parsed `SuperBlob` with
/// `CSMAGIC_EMBEDDED_SIGNATURE` magic embedded in a Mach-O binary. It is the
/// most common embedded signature data format you are likely to encounter.
pub struct EmbeddedSignature<'a> {
    /// Magic value from header.
    pub magic: CodeSigningMagic,
    /// Length of this super blob.
    pub length: u32,
    /// Number of blobs in this super blob.
    pub count: u32,

    /// Raw data backing this super blob.
    pub data: &'a [u8],

    /// All the blobs within this super blob.
    pub blobs: Vec<BlobEntry<'a>>,
}

impl<'a> std::fmt::Debug for EmbeddedSignature<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("SuperBlob")
            .field("magic", &self.magic)
            .field("length", &self.length)
            .field("count", &self.count)
            .field("blobs", &self.blobs)
            .finish()
    }
}

// There are other impl blocks for this structure in other modules.
impl<'a> EmbeddedSignature<'a> {
    /// Attempt to parse an embedded signature super blob from data.
    ///
    /// The argument to this function is likely the subset of the
    /// `__LINKEDIT` Mach-O section that the `LC_CODE_SIGNATURE` load instructions
    /// points it.
    pub fn from_bytes(data: &'a [u8]) -> Result<Self, MachOParseError> {
        let offset = &mut 0;

        // Parse the 3 fields from the SuperBlob.
        let magic = data.gread_with::<u32>(offset, scroll::BE)?.into();

        if magic != CodeSigningMagic::EmbeddedSignature {
            return Err(MachOParseError::BadMagic);
        }

        let length = data.gread_with(offset, scroll::BE)?;
        let count = data.gread_with(offset, scroll::BE)?;

        // Following the SuperBlob header is an array of .count BlobIndex defining
        // the Blob that follow.
        //
        // The BlobIndex doesn't declare the length of each Blob. However, it appears
        // the first 8 bytes of each blob contain the u32 magic and u32 length.
        // We do parse those here and set the blob length/slice accordingly. However,
        // we take an extra level of precaution by first computing a slice that doesn't
        // overrun into the next blob or past the end of the input buffer. This
        // helps detect invalid length advertisements in the blob payload.
        let mut blob_indices = Vec::with_capacity(count as usize);
        for _ in 0..count {
            blob_indices.push(data.gread_with::<BlobIndex>(offset, scroll::BE)?);
        }

        let mut blobs = Vec::with_capacity(blob_indices.len());

        for (i, index) in blob_indices.iter().enumerate() {
            let end_offset = if i == blob_indices.len() - 1 {
                data.len()
            } else {
                blob_indices[i + 1].offset as usize
            };

            let blob_data = &data[index.offset as usize..end_offset];

            let (magic, blob_length, _) = read_blob_header(blob_data)?;

            blobs.push(BlobEntry {
                index: i,
                slot: index.typ.into(),
                offset: index.offset as usize,
                magic: magic.into(),
                length: blob_length,
                data: blob_data,
            });
        }

        Ok(Self {
            magic,
            length,
            count,
            data,
            blobs,
        })
    }

    /// Find the first occurrence of the specified slot.
    pub fn find_slot(&self, slot: CodeSigningSlot) -> Option<&BlobEntry> {
        self.blobs.iter().find(|e| e.slot == slot)
    }

    pub fn find_slot_parsed(
        &self,
        slot: CodeSigningSlot,
    ) -> Result<Option<ParsedBlob<'_>>, MachOParseError> {
        if let Some(entry) = self.find_slot(slot) {
            Ok(Some(entry.clone().into_parsed_blob()?))
        } else {
            Ok(None)
        }
    }

    /// Attempt to resolve a parsed `CodeDirectoryBlob` for this signature data.
    ///
    /// Returns Err on data parsing error or if the blob slot didn't contain a code
    /// directory.
    ///
    /// Returns `Ok(None)` if there is no code directory slot.
    pub fn code_directory(&self) -> Result<Option<Box<CodeDirectoryBlob<'_>>>, MachOParseError> {
        if let Some(parsed) = self.find_slot_parsed(CodeSigningSlot::CodeDirectory)? {
            if let BlobData::CodeDirectory(cd) = parsed.blob {
                Ok(Some(cd))
            } else {
                Err(MachOParseError::BadMagic)
            }
        } else {
            Ok(None)
        }
    }

    /// Attempt to resolve a parsed `RequirementsBlob` for this signature data.
    ///
    /// Returns Err on data parsing error or if the blob slot didn't contain a requirements
    /// blob.
    ///
    /// Returns `Ok(None)` if there is no requirements slot.
    pub fn requirements(&self) -> Result<Option<RequirementsBlob<'_>>, MachOParseError> {
        if let Some(parsed) = self.find_slot_parsed(CodeSigningSlot::Requirements)? {
            if let BlobData::Requirements(reqs) = parsed.blob {
                Ok(Some(reqs))
            } else {
                Err(MachOParseError::BadMagic)
            }
        } else {
            Ok(None)
        }
    }

    /// Attempt to resolve raw signature data from `SignatureBlob`.
    ///
    /// The returned data is likely DER PKCS#7 with the root object
    /// pkcs7-signedData (1.2.840.113549.1.7.2).
    pub fn signature_data(&self) -> Result<Option<&'_ [u8]>, MachOParseError> {
        if let Some(parsed) = self.find_slot_parsed(CodeSigningSlot::Signature)? {
            if let BlobData::BlobWrapper(blob) = parsed.blob {
                Ok(Some(blob.data))
            } else {
                Err(MachOParseError::BadMagic)
            }
        } else {
            Ok(None)
        }
    }
}

/// Represents a single blob as defined by a `SuperBlob` index entry.
///
/// Instances have copies of their own index info, including the relative
/// order, slot type, and start offset within the `SuperBlob`.
///
/// The blob data is unparsed in this type. The blob payloads can be
/// turned into `ParsedBlob` via `.try_into()`.
#[derive(Clone)]
pub struct BlobEntry<'a> {
    /// Our blob index within the `SuperBlob`.
    pub index: usize,

    /// The slot type.
    pub slot: CodeSigningSlot,

    /// Our start offset within the `SuperBlob`.
    ///
    /// First byte is start of our magic.
    pub offset: usize,

    /// The magic value appearing at the beginning of the blob.
    pub magic: CodeSigningMagic,

    /// The length of the blob payload.
    pub length: usize,

    /// The raw data in this blob, including magic and length.
    pub data: &'a [u8],
}

impl<'a> std::fmt::Debug for BlobEntry<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("BlobEntry")
            .field("index", &self.index)
            .field("type", &self.slot)
            .field("offset", &self.offset)
            .field("length", &self.length)
            .field("magic", &self.magic)
            // .field("data", &self.data)
            .finish()
    }
}

impl<'a> BlobEntry<'a> {
    /// Attempt to convert to a `ParsedBlob`.
    pub fn into_parsed_blob(self) -> Result<ParsedBlob<'a>, MachOParseError> {
        self.try_into()
    }
}

/// Represents the parsed content of a blob entry.
#[derive(Debug)]
pub struct ParsedBlob<'a> {
    /// The blob record this blob came from.
    pub blob_entry: BlobEntry<'a>,

    /// The parsed blob data.
    pub blob: BlobData<'a>,
}

impl<'a> TryFrom<BlobEntry<'a>> for ParsedBlob<'a> {
    type Error = MachOParseError;

    fn try_from(blob_entry: BlobEntry<'a>) -> Result<Self, Self::Error> {
        let blob = BlobData::from_bytes(blob_entry.data)?;

        Ok(Self { blob_entry, blob })
    }
}

/// Represents a single, parsed Blob entry/slot.
///
/// Each variant corresponds to a CSMAGIC_* blob type.
#[derive(Debug)]
pub enum BlobData<'a> {
    Requirement(RequirementBlob<'a>),
    Requirements(RequirementsBlob<'a>),
    CodeDirectory(Box<CodeDirectoryBlob<'a>>),
    EmbeddedSignature(EmbeddedSignatureBlob<'a>),
    EmbeddedSignatureOld(EmbeddedSignatureOldBlob<'a>),
    EmbeddedEntitlements(EntitlementsBlob<'a>),
    DetachedSignature(DetachedSignatureBlob<'a>),
    BlobWrapper(BlobWrapperBlob<'a>),
    Other((u32, usize, &'a [u8])),
}

impl<'a> BlobData<'a> {
    /// Parse blob data by reading its magic and feeding into magic-specific parser.
    pub fn from_bytes(data: &'a [u8]) -> Result<Self, MachOParseError> {
        let (magic, length, _) = read_blob_header(data)?;

        // This should be a no-op. But it could (correctly) cause a panic if the
        // advertised length is incorrect and we would incur a buffer overrun.
        let data = &data[0..length];

        Ok(match magic {
            CSMAGIC_REQUIREMENT => Self::Requirement(RequirementBlob::from_bytes(data)?),
            CSMAGIC_REQUIREMENTS => Self::Requirements(RequirementsBlob::from_bytes(data)?),
            CSMAGIC_CODEDIRECTORY => {
                Self::CodeDirectory(Box::new(CodeDirectoryBlob::from_bytes(data)?))
            }
            CSMAGIC_EMBEDDED_SIGNATURE => {
                Self::EmbeddedSignature(EmbeddedSignatureBlob::from_bytes(data)?)
            }
            CSMAGIC_EMBEDDED_SIGNATURE_OLD => {
                Self::EmbeddedSignatureOld(EmbeddedSignatureOldBlob::from_bytes(data)?)
            }
            CSMAGIC_EMBEDDED_ENTITLEMENTS => {
                Self::EmbeddedEntitlements(EntitlementsBlob::from_bytes(data)?)
            }
            CSMAGIC_DETACHED_SIGNATURE => {
                Self::DetachedSignature(DetachedSignatureBlob::from_bytes(data)?)
            }
            CSMAGIC_BLOBWRAPPER => Self::BlobWrapper(BlobWrapperBlob::from_bytes(data)?),
            _ => Self::Other((magic, length, data)),
        })
    }
}

#[derive(Debug)]
pub enum Expression<'a> {
    False,
    True,
    Ident(&'a str),
    AppleAnchor,
    AnchorHash,
    InfoKeyValue,
    And(Box<Expression<'a>>, Box<Expression<'a>>),
    Or(Box<Expression<'a>>, Box<Expression<'a>>),
    CDHash,
    Not,
    InfoKeyField,
    CertField,
    TrustedCert,
    TrustedCerts,
    CertGeneric,
    AppleGenericAnchor,
    EntitlementField,
    Other(u32),
}

impl<'a> Expression<'a> {
    /// Parse an expression from bytes.
    pub fn from_bytes(data: &'a [u8]) -> Result<(Self, &'a [u8]), MachOParseError> {
        let offset = &mut 0;

        let tag: u32 = data.gread_with(offset, scroll::BE)?;

        let data = &data[*offset..];

        let instance = match tag {
            0 => Self::False,
            1 => Self::True,
            2 => Self::Ident(std::str::from_utf8(&data[*offset..])?),
            3 => Self::AppleAnchor,
            4 => Self::AnchorHash,
            5 => Self::InfoKeyValue,
            6 => {
                let (a, data) = Expression::from_bytes(data)?;
                let (b, data) = Expression::from_bytes(data)?;

                return Ok((Self::And(Box::new(a), Box::new(b)), data));
            }
            7 => {
                let (a, data) = Expression::from_bytes(data)?;
                let (b, data) = Expression::from_bytes(data)?;

                return Ok((Self::Or(Box::new(a), Box::new(b)), data));
            }
            8 => Self::CDHash,
            9 => Self::Not,
            10 => Self::InfoKeyField,
            11 => Self::CertField,
            12 => Self::TrustedCert,
            13 => Self::TrustedCerts,
            14 => Self::CertGeneric,
            15 => Self::AppleGenericAnchor,
            16 => Self::EntitlementField,
            _ => Self::Other(tag),
        };

        Ok((instance, data))
    }
}

/// Represents a Requirement blob (CSMAGIC_REQUIREMENT).
#[derive(Debug)]
pub struct RequirementBlob<'a> {
    pub expression: Expression<'a>,
}

impl<'a> RequirementBlob<'a> {
    /// Construct an instance by parsing bytes for a blob.
    ///
    /// Data contains magic and length header.
    pub fn from_bytes(data: &'a [u8]) -> Result<Self, MachOParseError> {
        let data = read_and_validate_blob_header(data, CSMAGIC_REQUIREMENT)?;

        let expression = Expression::from_bytes(data)?.0;

        Ok(Self { expression })
    }
}

/// Represents a Requirements blob (CSMAGIC_REQUIREMENTS).
#[derive(Debug)]
pub struct RequirementsBlob<'a> {
    segments: Vec<BlobData<'a>>,
}

impl<'a> RequirementsBlob<'a> {
    /// Construct an instance by parsing bytes for a blob.
    ///
    /// Data contains magic and length header.
    pub fn from_bytes(data: &'a [u8]) -> Result<Self, MachOParseError> {
        read_and_validate_blob_header(data, CSMAGIC_REQUIREMENTS)?;

        // There are other blobs nested within. A u32 denotes how many there are.
        // Then there is an array of N (u32, u32) denoting the type and
        // offset of each.
        let offset = &mut 8;
        let count = data.gread_with::<u32>(offset, scroll::BE)?;

        let mut indices = Vec::with_capacity(count as usize);
        for _ in 0..count {
            indices.push((
                data.gread_with::<u32>(offset, scroll::BE)?,
                data.gread_with::<u32>(offset, scroll::BE)?,
            ));
        }

        let mut segments = Vec::with_capacity(indices.len());

        for (i, (_, offset)) in indices.iter().enumerate() {
            let end_offset = if i == indices.len() - 1 {
                data.len()
            } else {
                indices[i + 1].1 as usize
            };

            let segment_data = &data[*offset as usize..end_offset];

            segments.push(BlobData::from_bytes(segment_data)?);
        }

        Ok(Self { segments })
    }
}

/// Represents a hash type from a CS_HASHTYPE_* constants.
#[derive(Clone, Copy, Debug)]
pub enum HashType {
    None,
    Sha1,
    Sha256,
    Sha256Truncated,
    Sha384,
    Unknown(u8),
}

impl From<u8> for HashType {
    fn from(v: u8) -> Self {
        match v {
            0 => Self::None,
            CS_HASHTYPE_SHA1 => Self::Sha1,
            CS_HASHTYPE_SHA256 => Self::Sha256,
            CS_HASHTYPE_SHA256_TRUNCATED => Self::Sha256Truncated,
            CS_HASHTYPE_SHA384 => Self::Sha384,
            _ => Self::Unknown(v),
        }
    }
}

impl Into<u8> for HashType {
    fn into(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Sha1 => CS_HASHTYPE_SHA1,
            Self::Sha256 => CS_HASHTYPE_SHA256,
            Self::Sha256Truncated => CS_HASHTYPE_SHA256_TRUNCATED,
            Self::Sha384 => CS_HASHTYPE_SHA384,
            Self::Unknown(v) => v,
        }
    }
}

impl HashType {
    /// Obtain a hasher for this digest type.
    pub fn as_hasher(&self) -> Result<ring::digest::Context, &'static str> {
        match self {
            Self::Sha1 => Ok(ring::digest::Context::new(
                &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
            )),
            Self::Sha256 | Self::Sha256Truncated => {
                Ok(ring::digest::Context::new(&ring::digest::SHA256))
            }
            Self::Sha384 => Ok(ring::digest::Context::new(&ring::digest::SHA384)),
            _ => Err("hasher not implemented"),
        }
    }

    /// Digest data given the configured hasher.
    pub fn digest(&self, data: &[u8]) -> Result<Vec<u8>, &'static str> {
        let mut hasher = self.as_hasher()?;

        hasher.update(data);
        let hash = hasher.finish().as_ref().to_vec();

        // TODO truncate hash.
        if matches!(self, Self::Sha256Truncated) {
            unimplemented!();
        }

        Ok(hash)
    }
}

pub struct Hash<'a> {
    pub data: &'a [u8],
}

impl<'a> Hash<'a> {
    pub fn to_vec(&self) -> Vec<u8> {
        self.data.to_vec()
    }
}

impl<'a> std::fmt::Debug for Hash<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&hex::encode(self.data))
    }
}

fn get_hashes(data: &[u8], offset: usize, count: usize, hash_size: usize) -> Vec<Hash<'_>> {
    data[offset..offset + (count * hash_size)]
        .chunks(hash_size)
        .map(|data| Hash { data })
        .collect()
}

/// Represents a code directory blob entry (CSSLOT_CODEDIRECTORY).
///
/// This struct is versioned and has been extended over time.
///
/// The struct here represents a superset of all fields in all versions.
///
/// The parser will set `Option<T>` fields to `None` for instances
/// where the version is lower than the version that field was introduced in.
#[derive(Debug)]
pub struct CodeDirectoryBlob<'a> {
    /// Compatibility version.
    pub version: u32,
    /// Setup and mode flags.
    pub flags: u32,
    /// Offset of hash slot element at index zero.
    pub hash_offset: u32,
    /// Offset of identifier string.
    pub ident_offset: u32,
    /// Number of special hash slots.
    pub n_special_slots: u32,
    /// Number of ordinary code hash slots.
    pub n_code_slots: u32,
    /// Limit to main image signature range.
    pub code_limit: u32,
    /// Size of each hash in bytes.
    pub hash_size: u8,
    /// Type of hash.
    pub hash_type: HashType,
    /// Platform identifier. 0 if not platform binary.
    pub platform: u8,
    /// Page size in bytes. (stored as log u8)
    pub page_size: u32,
    /// Unused (must be 0).
    pub spare2: u32,
    // Version 0x20100
    /// Offset of optional scatter vector.
    pub scatter_offset: Option<u32>,
    // Version 0x20200
    /// Offset of optional team identifier.
    pub team_offset: Option<u32>,
    // Version 0x20300
    /// Unused (must be 0).
    pub spare3: Option<u32>,
    /// Limit to main image signature range, 64 bits.
    pub code_limit_64: Option<u64>,
    // Version 0x20400
    /// Offset of executable segment.
    pub exec_seg_base: Option<u64>,
    /// Limit of executable segment.
    pub exec_seg_limit: Option<u64>,
    /// Executable segment flags.
    pub exec_seg_flags: Option<u64>,
    // Version 0x20500
    pub runtime: Option<u32>,
    pub pre_encrypt_offset: Option<u32>,
    // Version 0x20600
    pub linkage_hash_type: Option<u8>,
    pub linkage_truncated: Option<u8>,
    pub spare4: Option<u16>,
    pub linkage_offset: Option<u32>,
    pub linkage_size: Option<u32>,

    // End of blob header data / start of derived data.
    pub ident: &'a str,
    pub code_hashes: Vec<Hash<'a>>,
    pub special_hashes: HashMap<CodeSigningSlot, Hash<'a>>,
}

impl<'a> CodeDirectoryBlob<'a> {
    /// Construct an instance by parsing bytes for a blob.
    ///
    /// Data contains magic and length header.
    pub fn from_bytes(data: &'a [u8]) -> Result<Self, MachOParseError> {
        read_and_validate_blob_header(data, CSMAGIC_CODEDIRECTORY)?;

        let offset = &mut 8;

        let version = data.gread_with(offset, scroll::BE)?;
        let flags = data.gread_with(offset, scroll::BE)?;
        let hash_offset = data.gread_with(offset, scroll::BE)?;
        let ident_offset = data.gread_with::<u32>(offset, scroll::BE)?;
        let n_special_slots = data.gread_with(offset, scroll::BE)?;
        let n_code_slots = data.gread_with(offset, scroll::BE)?;
        let code_limit = data.gread_with(offset, scroll::BE)?;
        let hash_size = data.gread_with(offset, scroll::BE)?;
        let hash_type = data.gread_with::<u8>(offset, scroll::BE)?.into();
        let platform = data.gread_with(offset, scroll::BE)?;
        let page_size = data.gread_with::<u8>(offset, scroll::BE)?;
        let page_size = 2u32.pow(page_size as u32);
        let spare2 = data.gread_with(offset, scroll::BE)?;

        let scatter_offset = if version >= CS_SUPPORTSSCATTER {
            Some(data.gread_with(offset, scroll::BE)?)
        } else {
            None
        };
        let team_offset = if version >= CS_SUPPORTSTEAMID {
            Some(data.gread_with(offset, scroll::BE)?)
        } else {
            None
        };

        let (spare3, code_limit_64) = if version >= CS_SUPPORTSCODELIMIT64 {
            (
                Some(data.gread_with(offset, scroll::BE)?),
                Some(data.gread_with(offset, scroll::BE)?),
            )
        } else {
            (None, None)
        };

        let (exec_seg_base, exec_seg_limit, exec_seg_flags) = if version >= CS_SUPPORTSEXECSEG {
            (
                Some(data.gread_with(offset, scroll::BE)?),
                Some(data.gread_with(offset, scroll::BE)?),
                Some(data.gread_with(offset, scroll::BE)?),
            )
        } else {
            (None, None, None)
        };

        let (runtime, pre_encrypt_offset) = if version >= CS_SUPPORTSRUNTIME {
            (
                Some(data.gread_with(offset, scroll::BE)?),
                Some(data.gread_with(offset, scroll::BE)?),
            )
        } else {
            (None, None)
        };

        let (linkage_hash_type, linkage_truncated, spare4, linkage_offset, linkage_size) =
            if version >= CS_SUPPORTSLINKAGE {
                (
                    Some(data.gread_with(offset, scroll::BE)?),
                    Some(data.gread_with(offset, scroll::BE)?),
                    Some(data.gread_with(offset, scroll::BE)?),
                    Some(data.gread_with(offset, scroll::BE)?),
                    Some(data.gread_with(offset, scroll::BE)?),
                )
            } else {
                (None, None, None, None, None)
            };

        // Find trailing null in identifier string.
        let ident = match data[ident_offset as usize..]
            .split(|&b| b == 0)
            .map(std::str::from_utf8)
            .next()
        {
            Some(res) => res?,
            None => {
                return Err(MachOParseError::BadIdentifierString);
            }
        };

        let code_hashes = get_hashes(
            data,
            hash_offset as usize,
            n_code_slots as usize,
            hash_size as usize,
        );

        let special_hashes = get_hashes(
            data,
            (hash_offset - (hash_size as u32 * n_special_slots)) as usize,
            n_special_slots as usize,
            hash_size as usize,
        )
        .into_iter()
        .enumerate()
        .map(|(i, h)| (CodeSigningSlot::from(i as u32), h))
        .collect();

        Ok(Self {
            version,
            flags,
            hash_offset,
            ident_offset,
            n_special_slots,
            n_code_slots,
            code_limit,
            hash_size,
            hash_type,
            platform,
            page_size,
            spare2,
            scatter_offset,
            team_offset,
            spare3,
            code_limit_64,
            exec_seg_base,
            exec_seg_limit,
            exec_seg_flags,
            runtime,
            pre_encrypt_offset,
            linkage_hash_type,
            linkage_truncated,
            spare4,
            linkage_offset,
            linkage_size,
            ident,
            code_hashes,
            special_hashes,
        })
    }
}

/// Represents an embedded signature (CSMAGIC_EMBEDDED_SIGNATURE).
#[derive(Debug)]
pub struct EmbeddedSignatureBlob<'a> {
    data: &'a [u8],
}

impl<'a> EmbeddedSignatureBlob<'a> {
    /// Construct an instance by parsing bytes for a blob.
    ///
    /// Data contains magic and length header.
    pub fn from_bytes(data: &'a [u8]) -> Result<Self, MachOParseError> {
        Ok(Self {
            data: read_and_validate_blob_header(data, CSMAGIC_EMBEDDED_SIGNATURE)?,
        })
    }
}

/// An old embedded signature (CSMAGIC_EMBEDDED_SIGNATURE_OLD).
#[derive(Debug)]
pub struct EmbeddedSignatureOldBlob<'a> {
    data: &'a [u8],
}

impl<'a> EmbeddedSignatureOldBlob<'a> {
    /// Construct an instance by parsing bytes for a blob.
    ///
    /// Data contains magic and length header.
    pub fn from_bytes(data: &'a [u8]) -> Result<Self, MachOParseError> {
        Ok(Self {
            data: read_and_validate_blob_header(data, CSMAGIC_EMBEDDED_SIGNATURE_OLD)?,
        })
    }
}

/// Represents an Entitlements blob (CSSLOT_ENTITLEMENTS).
///
/// An entitlements blob contains an XML plist with a dict. Keys are
/// strings of the entitlements being requested and values appear to be
/// simple bools.  
#[derive(Debug)]
pub struct EntitlementsBlob<'a> {
    plist: &'a str,
}

impl<'a> EntitlementsBlob<'a> {
    /// Construct an instance by parsing bytes for a blob.
    ///
    /// Data contains magic and length header.
    pub fn from_bytes(data: &'a [u8]) -> Result<Self, MachOParseError> {
        let data = read_and_validate_blob_header(data, CSMAGIC_EMBEDDED_ENTITLEMENTS)?;
        let s = std::str::from_utf8(data)?;

        Ok(Self { plist: s })
    }
}

/// A detached signature (CSMAGIC_DETACHED_SIGNATURE).
#[derive(Debug)]
pub struct DetachedSignatureBlob<'a> {
    data: &'a [u8],
}

impl<'a> DetachedSignatureBlob<'a> {
    /// Construct an instance by parsing bytes for a blob.
    ///
    /// Data contains magic and length header.
    pub fn from_bytes(data: &'a [u8]) -> Result<Self, MachOParseError> {
        Ok(Self {
            data: read_and_validate_blob_header(data, CSMAGIC_DETACHED_SIGNATURE)?,
        })
    }
}

/// Represents a generic blob wrapper (CSMAGIC_BLOBWRAPPER).
pub struct BlobWrapperBlob<'a> {
    data: &'a [u8],
}

impl<'a> BlobWrapperBlob<'a> {
    /// Construct an instance by parsing bytes for a blob.
    ///
    /// Data contains magic and length header.
    pub fn from_bytes(data: &'a [u8]) -> Result<Self, MachOParseError> {
        Ok(Self {
            data: read_and_validate_blob_header(data, CSMAGIC_BLOBWRAPPER)?,
        })
    }
}

impl<'a> std::fmt::Debug for BlobWrapperBlob<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{}", hex::encode(self.data)))
    }
}

#[repr(C)]
pub struct Scatter {
    /// Number of pages. 0 for sentinel only.
    count: u32,
    /// First page number.
    base: u32,
    /// Offset in target.
    target_offset: u64,
    /// Reserved.
    spare: u64,
}

#[derive(Debug)]
pub enum MachOParseError {
    MissingLinkedit,
    BadMagic,
    ScrollError(scroll::Error),
    Utf8Error(std::str::Utf8Error),
    BadIdentifierString,
}

impl std::fmt::Display for MachOParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingLinkedit => f.write_fmt(format_args!(
                "unable to locate {} segment despite load command reference",
                SEG_LINKEDIT,
            )),
            Self::BadMagic => f.write_str("bad magic value in SuperBlob header"),
            Self::ScrollError(e) => e.fmt(f),
            Self::Utf8Error(e) => e.fmt(f),
            Self::BadIdentifierString => f.write_str("identifier string isn't null terminated"),
        }
    }
}

impl std::error::Error for MachOParseError {}

impl From<scroll::Error> for MachOParseError {
    fn from(e: scroll::Error) -> Self {
        Self::ScrollError(e)
    }
}

impl From<std::str::Utf8Error> for MachOParseError {
    fn from(e: std::str::Utf8Error) -> Self {
        Self::Utf8Error(e)
    }
}

/// Describes signature data embedded within a Mach-O binary.
pub struct MachOSignatureData<'a> {
    /// The number of segments in the Mach-O binary.
    pub segments_count: usize,

    /// Which segment offset is the `__LINKEDIT` segment.
    pub linkedit_segment_index: usize,

    /// The start offset of the signature data within the `__LINKEDIT` segment.
    pub signature_start_offset: usize,

    /// The end offset of the signature data within the `__LINKEDIT` segment.
    pub signature_end_offset: usize,

    /// Raw data in the `__LINKEDIT` segment.
    pub linkedit_segment_data: &'a [u8],

    /// The signature data within the `__LINKEDIT` segment.
    pub signature_data: &'a [u8],
}

/// Attempt to extract a reference to raw signature data in a Mach-O binary.
///
/// An `LC_CODE_SIGNATURE` load command in the Mach-O file header points to
/// signature data in the `__LINKEDIT` segment.
///
/// This function is used as part of parsing signature data. You probably want to
/// use a function that parses referenced data.
pub fn find_signature_data<'a>(
    obj: &'a MachO,
) -> Result<Option<MachOSignatureData<'a>>, MachOParseError> {
    if let Some(linkedit_data_command) = obj.load_commands.iter().find_map(|load_command| {
        if let CommandVariant::CodeSignature(command) = &load_command.command {
            Some(command)
        } else {
            None
        }
    }) {
        // Now find the slice of data in the __LINKEDIT segment we need to parse.
        let (linkedit_segment_index, linkedit) = obj
            .segments
            .iter()
            .enumerate()
            .find(|(_, segment)| {
                if let Ok(name) = segment.name() {
                    name == SEG_LINKEDIT
                } else {
                    false
                }
            })
            .ok_or(MachOParseError::MissingLinkedit)?;

        let signature_start_offset =
            linkedit_data_command.dataoff as usize - linkedit.fileoff as usize;
        let signature_end_offset = signature_start_offset + linkedit_data_command.datasize as usize;

        let signature_data = &linkedit.data[signature_start_offset..signature_end_offset];

        Ok(Some(MachOSignatureData {
            segments_count: obj.segments.len(),
            linkedit_segment_index,
            signature_start_offset,
            signature_end_offset,
            linkedit_segment_data: linkedit.data,
            signature_data,
        }))
    } else {
        Ok(None)
    }
}

/// Parse raw Mach-O signature data into a data structure.
///
/// The source data likely came from the `__LINKEDIT` segment and was
/// discovered via `find_signature_data()`.
///
/// Only a high-level parse of the super blob and its blob indices is performed:
/// the parser does not look inside individual blob payloads.
pub fn parse_signature_data(data: &[u8]) -> Result<EmbeddedSignature<'_>, MachOParseError> {
    let magic: u32 = data.pread_with(0, scroll::BE)?;

    if magic == CSMAGIC_EMBEDDED_SIGNATURE {
        EmbeddedSignature::from_bytes(data)
    } else {
        Err(MachOParseError::BadMagic)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        cryptographic_message_syntax::SignedData,
        std::{
            io::Read,
            path::{Path, PathBuf},
        },
    };

    const MACHO_UNIVERSAL_MAGIC: [u8; 4] = [0xca, 0xfe, 0xba, 0xbe];
    const MACHO_64BIT_MAGIC: [u8; 4] = [0xfe, 0xed, 0xfa, 0xcf];

    /// Find files in a directory appearing to be Mach-O by sniffing magic.
    ///
    /// Ignores file I/O errors.
    fn find_likely_macho_files(path: &Path) -> Vec<PathBuf> {
        let mut res = Vec::new();

        let dir = std::fs::read_dir(path).unwrap();

        for entry in dir {
            let entry = entry.unwrap();

            if let Ok(mut fh) = std::fs::File::open(&entry.path()) {
                let mut magic = [0; 4];

                if let Ok(size) = fh.read(&mut magic) {
                    if size == 4 && (magic == MACHO_UNIVERSAL_MAGIC || magic == MACHO_64BIT_MAGIC) {
                        res.push(entry.path());
                    }
                }
            }
        }

        res
    }

    fn find_apple_codesign_signature(macho: &goblin::mach::MachO) -> Option<Vec<u8>> {
        if let Ok(Some(codesign_data)) = find_signature_data(macho) {
            if let Ok(signature) = parse_signature_data(codesign_data.signature_data) {
                if let Ok(Some(data)) = signature.signature_data() {
                    Some(data.to_vec())
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Attempt to extract CMS signature data from Mach-O binaries in a given path.
    fn find_macho_codesign_signatures_in_dir(directory: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut res = Vec::new();

        for path in find_likely_macho_files(directory).into_iter() {
            if let Ok(file_data) = std::fs::read(&path) {
                if let Ok(mach) = goblin::mach::Mach::parse(&file_data) {
                    match mach {
                        goblin::mach::Mach::Binary(macho) => {
                            if let Some(cms_data) = find_apple_codesign_signature(&macho) {
                                res.push((path, cms_data));
                            }
                        }
                        goblin::mach::Mach::Fat(multiarch) => {
                            for i in 0..multiarch.narches {
                                if let Ok(macho) = multiarch.get(i) {
                                    if let Some(cms_data) = find_apple_codesign_signature(&macho) {
                                        res.push((path.clone(), cms_data));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        res
    }

    fn parse_macho_cms_data_in_dir(dir: &Path) {
        println!("searching for Mach-O files in {}", dir.display());
        for (path, cms_data) in find_macho_codesign_signatures_in_dir(dir) {
            cryptographic_message_syntax::asn1::rfc5652::SignedData::decode_ber(&cms_data).unwrap();

            match SignedData::parse_ber(&cms_data) {
                Ok(signed_data) => {
                    for signer in signed_data.signers() {
                        if let Err(e) = signer.verify_signature_with_signed_data(&signed_data) {
                            println!(
                                "signature verification failed for {}: {}",
                                path.display(),
                                e
                            )
                        }

                        if let Ok(()) = signer.verify_message_digest_with_signed_data(&signed_data)
                        {
                            println!(
                                "message digest verification unexpectedly correct for {}",
                                path.display()
                            )
                        }
                    }
                }
                Err(e) => {
                    println!(
                        "error performing high-level parse of {}: {:?}",
                        path.display(),
                        e
                    );
                }
            }
        }
    }

    #[test]
    fn parse_applications_macho_signatures() {
        // This test scans common directories containing Mach-O files on macOS and
        // verifies we can parse CMS blobs within.

        if let Ok(dir) = std::fs::read_dir("/Applications") {
            for entry in dir {
                let entry = entry.unwrap();

                let search_dir = entry.path().join("Contents").join("MacOS");

                if search_dir.exists() {
                    parse_macho_cms_data_in_dir(&search_dir);
                }
            }
        }

        for dir in &["/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"] {
            let dir = PathBuf::from(dir);

            if dir.exists() {
                parse_macho_cms_data_in_dir(&dir);
            }
        }
    }
}