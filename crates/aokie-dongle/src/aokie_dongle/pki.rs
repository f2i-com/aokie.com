#![cfg(target_os = "windows")]
//! Catalog generation + self-signed code-signing for the Aokie WinUSB
//! driver package.
//!
//! Ported from libwdi-1.5.1 `pki.c` (Pete Batard, GNU LGPL). The flow:
//!
//! 1. `create_cat` opens a fresh `.cat` file, sets the `HWID1` and `OS`
//!    cat-level attributes, hashes each member file (the `.inf`) with the
//!    Authenticode SIP, and persists the sorted store. Mirrors libwdi's
//!    `CreateCat` + `AddFileHash`.
//! 2. `self_sign_file` generates an RSA-2048 keypair in the machine
//!    keyset (libwdi's choice — sufficient for a one-shot driver
//!    self-sign and noticeably faster to generate than 4096 during
//!    the interactive UAC flow), creates a self-signed code-signing
//!    certificate, adds the cert to LocalMachine\Root and
//!    LocalMachine\TrustedPublisher, signs the `.cat` via
//!    `SignerSignEx` with SHA-256, and finally destroys the private
//!    key so it can't be reused. Mirrors libwdi's `SelfSignFile` +
//!    `DeletePrivateKey` + `CreateSelfSignedCert`.
//!
//! Without this, Windows 10/11 installs fail with
//! `ERROR_FILE_HASH_NOT_IN_CATALOG` (0xE000024B) or
//! `ERROR_NO_CATALOG_FOR_OEM_INF` (0xE000022F): the INF references a
//! `.cat` and Windows demands one with valid file-hashes signed by a
//! cert it trusts.

use std::ffi::CString;
use std::mem::{size_of, zeroed};
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::core::{GUID, PCSTR, PSTR, PWSTR};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE, NTE_BAD_KEYSET,
    NTE_KEYSET_ENTRY_BAD, TRUE,
};
use windows_sys::Win32::Security::Cryptography::Catalog::{
    CryptCATAdminCalcHashFromFileHandle, CryptCATClose, CryptCATOpen, CryptCATPersistStore,
    CryptCATPutAttrInfo, CryptCATPutCatAttrInfo, CryptCATPutMemberInfo, CRYPTCATMEMBER,
    CRYPTCAT_OPEN_CREATENEW,
};
use windows_sys::Win32::Security::Cryptography::{
    CertAddCertificateContextToStore, CertCloseStore, CertCreateSelfSignCertificate,
    CertDeleteCertificateFromStore, CertDuplicateCertificateContext, CertEnumCertificatesInStore,
    CertFindCertificateInStore, CertFreeCertificateContext, CertGetCertificateContextProperty,
    CertGetNameStringA, CertOpenStore, CertSetCertificateContextProperty, CertStrToNameA,
    CryptAcquireContextW, CryptDestroyKey, CryptEncodeObjectEx, CryptGenKey, CryptReleaseContext,
    SignerFreeSignerContext, SignerSignEx, AT_SIGNATURE, CERT_CONTEXT, CERT_EXTENSION,
    CERT_EXTENSIONS, CERT_FIND_SUBJECT_NAME, CERT_FRIENDLY_NAME_PROP_ID,
    CERT_NAME_SIMPLE_DISPLAY_TYPE, CERT_SHA1_HASH_PROP_ID, CERT_STORE_ADD_REPLACE_EXISTING,
    CERT_STORE_PROV_SYSTEM_W, CERT_SYSTEM_STORE_LOCAL_MACHINE_ID, CERT_SYSTEM_STORE_LOCATION_SHIFT,
    CERT_X500_NAME_STR, CRYPT_ATTRIBUTE, CRYPT_ATTRIBUTES, CRYPT_DELETEKEYSET, CRYPT_INTEGER_BLOB,
    CRYPT_KEY_PROV_INFO, CRYPT_MACHINE_KEYSET, CRYPT_NEWKEYSET, CRYPT_SILENT, CRYPT_VERIFYCONTEXT,
    PROV_RSA_FULL, SIGNER_CERT, SIGNER_CERT_0, SIGNER_CERT_STORE_INFO, SIGNER_FILE_INFO,
    SIGNER_SIGNATURE_INFO, SIGNER_SIGNATURE_INFO_0, SIGNER_SUBJECT_INFO, SIGNER_SUBJECT_INFO_0,
    X509_ASN_ENCODING, X509_ENHANCED_KEY_USAGE,
};

// CERT_NAME_BLOB and CRYPT_DATA_BLOB are typedefs of CRYPT_INTEGER_BLOB
// in wincrypt.h; windows-sys exposes only the underlying struct, so we
// reuse it under the SDK names for clarity at call sites.
#[allow(non_camel_case_types)]
type CERT_NAME_BLOB = CRYPT_INTEGER_BLOB;
#[allow(non_camel_case_types)]
type CRYPT_DATA_BLOB = CRYPT_INTEGER_BLOB;

/// `CERT_ENHKEY_USAGE` — windows-sys 0.59 doesn't ship the struct, only
/// the field pattern (an array of OIDs). The encoding is `cbItems` +
/// `prgItems[]`, matching wincrypt.h.
#[repr(C)]
struct CertEnhkeyUsage {
    c_usage_identifier: u32,
    rgpsz_usage_identifier: *mut PSTR,
}
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, OPEN_EXISTING,
};

const SHA1_HASH_LENGTH: usize = 20;

const CRYPTCAT_ATTR_AUTHENTICATED: u32 = 0x10000000;
const CRYPTCAT_ATTR_NAMEASCII: u32 = 0x00000001;
const CRYPTCAT_ATTR_DATAASCII: u32 = 0x00010000;

/// SHA-256 with RSA OID, used as the self-signed cert's signature
/// algorithm. Windows 10+ prefers SHA-256 over SHA-1 for new certs.
const SZ_OID_RSA_SHA256RSA: &str = "1.2.840.113549.1.1.11";
/// `szOID_ENHANCED_KEY_USAGE` — extension OID for the EKU we attach to
/// the self-signed cert (limits its use to code signing).
const SZ_OID_ENHANCED_KEY_USAGE: &str = "2.5.29.37";
/// Code-signing EKU OID — the only thing we want this cert to be
/// usable for.
const SZ_OID_PKIX_KP_CODE_SIGNING: &str = "1.3.6.1.5.5.7.3.3";

/// Cat member SIP OIDs. INF members use the CAB-data OID (libwdi/inf2cat
/// convention) plus the SHA-1 digest OID — the cat-level signature is
/// SHA-256 (via SignerSignEx) but per-member hashes stay on SHA-1 to
/// match the windows-7-era SIP indirect-data layout that Windows 10
/// still accepts.
const SPC_CAB_DATA_OBJID: &str = "1.3.6.1.4.1.311.2.1.25";
const SZ_OID_OIWSEC_SHA1: &str = "1.3.14.3.2.26";

/// Authenticode auth attributes that every signed catalog should carry.
/// `SPC_SP_OPUS_INFO_OBJID` plus `SPC_STATEMENT_TYPE_OBJID` are what
/// signtool emits for any code-signed file; libwdi mirrors that and
/// without them some signature verifiers (and certain code-integrity
/// policies) reject the cat as non-Authenticode-conforming.
const SPC_SP_OPUS_INFO_OBJID: &str = "1.3.6.1.4.1.311.2.1.12";
const SPC_STATEMENT_TYPE_OBJID: &str = "1.3.6.1.4.1.311.2.1.11";

/// Pre-encoded ASN.1 DER for the authenticode opus-info attribute
/// (an empty SEQUENCE — fields are all optional).
const SP_OPUS_INFO_DATA: [u8; 2] = [0x30, 0x00];

/// Pre-encoded ASN.1 DER for the statement-type attribute, identifying
/// this as `SPC_INDIVIDUAL_SP_KEY_PURPOSE_OBJID` (1.3.6.1.4.1.311.2.1.21
/// — non-commercial individual). Same magic libwdi ships verbatim.
const STATEMENT_TYPE_DATA: [u8; 14] = [
    0x30, 0x0C, 0x06, 0x0A, 0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0x37, 0x02, 0x01, 0x15,
];

/// SPC link choice for a `pwszFile`-style placeholder ("Obsolete"). The
/// SPC_LINK union has three variants {URL=1, Moniker=2, File=3}; for
/// every cat member libwdi emits, the choice is FILE — even though it
/// then assigns the literal `<<<Obsolete>>>` bytes via the URL/File
/// union slot (same offset). The choice value is what Windows actually
/// inspects when verifying, so it must be 3.
const SPC_FILE_LINK_CHOICE: u32 = 3;

/// Subject-info GUID for INF members in a catalog. Same value as
/// libwdi's `inf_guid`.
const INF_SUBJECT_GUID: GUID = GUID {
    data1: 0xDE351A42,
    data2: 0x8E59,
    data3: 0x11D0,
    data4: [0x8C, 0x47, 0x00, 0xC0, 0x4F, 0xC2, 0x95, 0xEE],
};

const SIGNER_SUBJECT_FILE: u32 = 0x01;
const SIGNER_CERT_STORE: u32 = 0x02;
const SIGNER_CERT_POLICY_CHAIN: u32 = 0x02;
const SIGNER_NO_ATTR: u32 = 0x00;
/// CALG_SHA_256 — wincrypt.h ALG_ID for SHA-256.
const CALG_SHA_256: u32 = 0x0000800c;

/// Local machine-keyset key container name. Same key is reused across
/// installs and destroyed after every successful sign (see
/// `destroy_signing_keypair`).
const KEY_CONTAINER_NAME: &str = "aokie WinUSB driver-signing key";

/// Generate a `.cat` next to the given INF and self-sign it. Returns
/// the cat path on success.
pub fn create_and_sign_cat(
    inf_path: &Path,
    hardware_id: &str,
) -> Result<std::path::PathBuf, String> {
    let cat_path = derive_cat_path(inf_path)?;
    create_cat(&cat_path, hardware_id, inf_path)?;
    let cert_subject = format!("CN={} (aokie autogenerated)", hardware_id);
    self_sign_file(&cat_path, &cert_subject)?;
    Ok(cat_path)
}

pub(crate) fn derive_cat_path(inf_path: &Path) -> Result<std::path::PathBuf, String> {
    inf_path
        .extension()
        .and_then(|ext| ext.to_str())
        .filter(|ext| ext.eq_ignore_ascii_case("inf"))
        .ok_or_else(|| format!("expected .inf path, got {:?}", inf_path))?;
    Ok(inf_path.with_extension("cat"))
}

// ---------------------------------------------------------------------
// CAT generation
// ---------------------------------------------------------------------

fn create_cat(cat_path: &Path, hardware_id: &str, inf_path: &Path) -> Result<(), String> {
    // CRYPTCAT_OPEN_CREATENEW fails if the file exists. Best-effort
    // delete; if it can't be removed we'll surface the open error.
    let _ = std::fs::remove_file(cat_path);

    let mut hprov: usize = 0;
    if unsafe {
        CryptAcquireContextW(
            &mut hprov,
            null(),
            null(),
            PROV_RSA_FULL,
            CRYPT_VERIFYCONTEXT,
        )
    } == 0
    {
        return Err(format!(
            "CryptAcquireContext(VERIFYCONTEXT) failed: Win32 error {}",
            unsafe { GetLastError() }
        ));
    }
    let _hprov_guard = HCryptProvGuard(hprov);

    let cat_path_w = wide_null(&cat_path.to_string_lossy());
    // Pass 0/0 for dwPublicVersion and dwEncodingType, exactly like
    // libwdi's CreateCat. Per MSDN, dwEncodingType=0 expands to
    // X509_ASN_ENCODING | PKCS_7_ASN_ENCODING (0x10001) — passing
    // X509_ASN_ENCODING alone (0x1) creates a cat without the PKCS_7
    // layer, which CryptCATPutMemberInfo accepts but CryptCATPersistStore
    // later rejects with ERROR_INVALID_PARAMETER (the "Reinstall WinUSB
    // Driver" failure mode). dwPublicVersion=0 picks the default 0x100
    // (== CRYPTCAT_VERSION_1) the same way libwdi does.
    let hcat = unsafe { CryptCATOpen(cat_path_w.as_ptr(), CRYPTCAT_OPEN_CREATENEW, hprov, 0, 0) };
    if hcat == INVALID_HANDLE_VALUE {
        return Err(format!(
            "CryptCATOpen({:?}) failed: Win32 error {}",
            cat_path,
            unsafe { GetLastError() }
        ));
    }
    let _cat_guard = CatGuard(hcat);

    // HWID1 attribute — the device hardware id this cat is for.
    let mut hwid_w = wide_lower_z(hardware_id);
    let hwid_bytes_len = (hwid_w.len() * 2) as u32;
    let put_ok = unsafe {
        CryptCATPutCatAttrInfo(
            hcat,
            wide_z("HWID1").as_ptr(),
            CRYPTCAT_ATTR_AUTHENTICATED | CRYPTCAT_ATTR_NAMEASCII | CRYPTCAT_ATTR_DATAASCII,
            hwid_bytes_len,
            hwid_w.as_mut_ptr() as *mut u8,
        )
    };
    if put_ok.is_null() {
        return Err(format!(
            "CryptCATPutCatAttrInfo(HWID1) failed: Win32 error {}",
            unsafe { GetLastError() }
        ));
    }

    // OS attribute — declares which Windows versions the cat targets.
    let mut os_w = wide_lower_z("7_X86,7_X64,8_X86,8_X64,8_ARM,10_X86,10_X64,10_ARM,10_ARM64");
    let os_bytes_len = (os_w.len() * 2) as u32;
    let put_ok = unsafe {
        CryptCATPutCatAttrInfo(
            hcat,
            wide_z("OS").as_ptr(),
            CRYPTCAT_ATTR_AUTHENTICATED | CRYPTCAT_ATTR_NAMEASCII | CRYPTCAT_ATTR_DATAASCII,
            os_bytes_len,
            os_w.as_mut_ptr() as *mut u8,
        )
    };
    if put_ok.is_null() {
        return Err(format!(
            "CryptCATPutCatAttrInfo(OS) failed: Win32 error {}",
            unsafe { GetLastError() }
        ));
    }

    // INF as the only catalog member.
    let inf_hash = sha1_authenticode_hash(inf_path)?;
    let inf_file_name = inf_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("inf path has no file name: {:?}", inf_path))?
        .to_lowercase();
    add_inf_member(hcat, &inf_file_name, &inf_hash)?;

    if unsafe { CryptCATPersistStore(hcat) } == 0 {
        return Err(format!(
            "CryptCATPersistStore failed: Win32 error {}",
            unsafe { GetLastError() }
        ));
    }
    Ok(())
}

fn sha1_authenticode_hash(path: &Path) -> Result<[u8; SHA1_HASH_LENGTH], String> {
    let path_w = wide_null(&path.to_string_lossy());
    let h = unsafe {
        CreateFileW(
            path_w.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ,
            null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(format!(
            "CreateFile({:?}) failed: Win32 error {}",
            path,
            unsafe { GetLastError() }
        ));
    }
    let _file_guard = FileGuard(h);

    let mut hash = [0u8; SHA1_HASH_LENGTH];
    let mut hash_len = SHA1_HASH_LENGTH as u32;
    if unsafe { CryptCATAdminCalcHashFromFileHandle(h, &mut hash_len, hash.as_mut_ptr(), 0) } == 0 {
        return Err(format!(
            "CryptCATAdminCalcHashFromFileHandle({:?}) failed: Win32 error {}",
            path,
            unsafe { GetLastError() }
        ));
    }
    Ok(hash)
}

#[repr(C)]
struct SpcLinkUrl {
    d_link_choice: u32,
    pwsz_url: PWSTR,
}

#[repr(C)]
struct CryptObjectIdBlob {
    cb_data: u32,
    pb_data: *mut u8,
}

#[repr(C)]
struct CryptAlgorithmIdentifierLocal {
    psz_obj_id: PSTR,
    parameters: CryptObjectIdBlob,
}

#[repr(C)]
struct CryptAttrTypeValue {
    psz_obj_id: PSTR,
    value: CryptObjectIdBlob,
}

#[repr(C)]
struct SipIndirectData {
    data: CryptAttrTypeValue,
    digest_algorithm: CryptAlgorithmIdentifierLocal,
    digest_cb: u32,
    digest_pb: *mut u8,
}

fn add_inf_member(
    hcat: HANDLE,
    inf_file_name: &str,
    inf_hash: &[u8; SHA1_HASH_LENGTH],
) -> Result<(), String> {
    // The cat-member SIP needs a populated link, even though for INF
    // members the value is the literal placeholder "<<<Obsolete>>>".
    // The link CHOICE is FILE = 3 (not URL = 1) — that's what Windows
    // checks when it traverses the cat for verification.
    let mut obsolete_w = wide_null("<<<Obsolete>>>");
    let mut spc_link = SpcLinkUrl {
        d_link_choice: SPC_FILE_LINK_CHOICE,
        pwsz_url: obsolete_w.as_mut_ptr(),
    };
    let inf_data_oid = cstring(SPC_CAB_DATA_OBJID);
    let mut spc_data_blob = encode_spc_link(&mut spc_link, &inf_data_oid)?;

    let sha1_oid = cstring(SZ_OID_OIWSEC_SHA1);
    let mut hash_copy = *inf_hash;

    let mut sip_indirect = SipIndirectData {
        data: CryptAttrTypeValue {
            psz_obj_id: inf_data_oid.as_ptr() as PSTR,
            value: CryptObjectIdBlob {
                cb_data: spc_data_blob.len() as u32,
                pb_data: spc_data_blob.as_mut_ptr(),
            },
        },
        digest_algorithm: CryptAlgorithmIdentifierLocal {
            psz_obj_id: sha1_oid.as_ptr() as PSTR,
            parameters: CryptObjectIdBlob {
                cb_data: 0,
                pb_data: null_mut(),
            },
        },
        digest_cb: SHA1_HASH_LENGTH as u32,
        digest_pb: hash_copy.as_mut_ptr(),
    };

    // Reference tag = uppercase hex SHA-1, matching what inf2cat
    // produces. Windows uses this as the cat's lookup key for the file.
    let mut hex_tag = String::with_capacity(SHA1_HASH_LENGTH * 2);
    for b in inf_hash.iter() {
        hex_tag.push_str(&format!("{:02X}", b));
    }
    let mut hex_tag_w = wide_null(&hex_tag);

    // pwszFileName=NULL matches libwdi's AddFileHash. The filename is
    // carried by the "File" CRYPTCAT_ATTR_INFO we add immediately after
    // — passing it both via the member's filename field AND as a
    // separate attribute creates a duplicated/ambiguous member that
    // CryptCATPersistStore can reject. Reference tag (the SHA-1 hex)
    // is the cat's lookup key; that's what Windows actually uses.
    let pcat_member: *mut CRYPTCATMEMBER = unsafe {
        let mut subject_guid = INF_SUBJECT_GUID;
        CryptCATPutMemberInfo(
            hcat,
            null_mut(),
            hex_tag_w.as_mut_ptr(),
            &mut subject_guid,
            0x200,
            size_of::<SipIndirectData>() as u32,
            &mut sip_indirect as *mut _ as *mut u8,
        )
    };
    if pcat_member.is_null() {
        return Err(format!(
            "CryptCATPutMemberInfo({}) failed: Win32 error {}",
            inf_file_name,
            unsafe { GetLastError() }
        ));
    }

    let mut file_attr_w = wide_null(inf_file_name);
    let attr_ok = unsafe {
        CryptCATPutAttrInfo(
            hcat,
            pcat_member,
            wide_z("File").as_ptr(),
            CRYPTCAT_ATTR_AUTHENTICATED | CRYPTCAT_ATTR_NAMEASCII | CRYPTCAT_ATTR_DATAASCII,
            (file_attr_w.len() * 2) as u32,
            file_attr_w.as_mut_ptr() as *mut u8,
        )
    };
    if attr_ok.is_null() {
        return Err(format!(
            "CryptCATPutAttrInfo(File) failed: Win32 error {}",
            unsafe { GetLastError() }
        ));
    }

    let mut osattr_w = wide_null("2:5.1,2:5.2,2:6.0,2:6.1,2:6.2,2:6.3,2:10.0");
    let attr_ok = unsafe {
        CryptCATPutAttrInfo(
            hcat,
            pcat_member,
            wide_z("OSAttr").as_ptr(),
            CRYPTCAT_ATTR_AUTHENTICATED | CRYPTCAT_ATTR_NAMEASCII | CRYPTCAT_ATTR_DATAASCII,
            (osattr_w.len() * 2) as u32,
            osattr_w.as_mut_ptr() as *mut u8,
        )
    };
    if attr_ok.is_null() {
        return Err(format!(
            "CryptCATPutAttrInfo(OSAttr) failed: Win32 error {}",
            unsafe { GetLastError() }
        ));
    }

    Ok(())
}

fn encode_spc_link(spc_link: &mut SpcLinkUrl, type_oid: &CString) -> Result<Vec<u8>, String> {
    let mut size: u32 = 0;
    let ok = unsafe {
        CryptEncodeObjectEx(
            X509_ASN_ENCODING,
            type_oid.as_ptr() as PCSTR,
            spc_link as *mut _ as *const std::ffi::c_void,
            0,
            null(),
            null_mut(),
            &mut size,
        )
    };
    if ok == 0 || size == 0 {
        return Err(format!(
            "CryptEncodeObjectEx(spc_link, sizing) failed: Win32 error {}",
            unsafe { GetLastError() }
        ));
    }
    let mut buf = vec![0u8; size as usize];
    let ok = unsafe {
        CryptEncodeObjectEx(
            X509_ASN_ENCODING,
            type_oid.as_ptr() as PCSTR,
            spc_link as *mut _ as *const std::ffi::c_void,
            0,
            null(),
            buf.as_mut_ptr() as *mut std::ffi::c_void,
            &mut size,
        )
    };
    if ok == 0 {
        return Err(format!(
            "CryptEncodeObjectEx(spc_link) failed: Win32 error {}",
            unsafe { GetLastError() }
        ));
    }
    buf.truncate(size as usize);
    Ok(buf)
}

// ---------------------------------------------------------------------
// Self-signed cert + signing
// ---------------------------------------------------------------------

#[repr(C)]
struct SignatureAlgorithmLocal {
    psz_obj_id: PSTR,
    parameters: CryptObjectIdBlob,
}

fn self_sign_file(path: &Path, cert_subject: &str) -> Result<(), String> {
    // Clean up any prior cert with this subject from both stores
    // before adding a fresh one. libwdi does this so the user's
    // certificate stores don't accumulate one cert per re-install
    // (each install generates a new keypair, so even ADD_REPLACE_EXISTING
    // doesn't collapse them — different public keys = different certs).
    let _ = remove_cert_from_store(cert_subject, "Root");
    let _ = remove_cert_from_store(cert_subject, "TrustedPublisher");

    let cert = create_self_signed_cert(cert_subject)?;
    // Destroy the keypair on every exit path. Earlier this code only
    // called destroy_signing_keypair() on the success branch and the
    // sign-cat-with-cert failure path — an `add_cert_to_store` failure
    // would early-return without cleanup, leaving the RSA-2048
    // private key sitting in LocalMachine\…\KEY_CONTAINER_NAME for an
    // attacker to discover and re-sign with. The guard fires from
    // every return below.
    let _key_cleanup = KeypairCleanupGuard;
    if let Err(e) = add_cert_to_store(cert.0, "Root") {
        cert_free(cert.0);
        return Err(e);
    }
    if let Err(e) = add_cert_to_store(cert.0, "TrustedPublisher") {
        cert_free(cert.0);
        return Err(e);
    }

    let sign_result = sign_cat_with_cert(path, cert.0);
    cert_free(cert.0);
    sign_result
}

/// Drop guard that destroys the self-signed driver-signing keypair
/// from the machine keyset. The cert public key remains in the user's
/// Trusted Publishers / Root store (Windows needs it there to verify
/// the .cat at install time), but without the private key on disk, no
/// one can use that cert to sign anything new.
struct KeypairCleanupGuard;
impl Drop for KeypairCleanupGuard {
    fn drop(&mut self) {
        if let Err(e) = destroy_signing_keypair() {
            eprintln!("[pki] destroy_signing_keypair via guard failed: {}", e);
        }
    }
}

/// Delete every cert in `store_name` whose subject matches
/// `cert_subject`. Best-effort — failures are logged but don't abort
/// the install (the new add will end up next to the stale ones, which
/// is harmless from a verification standpoint).
fn remove_cert_from_store(cert_subject: &str, store_name: &str) -> Result<usize, String> {
    let store_name_w = wide_null(store_name);
    const CERT_SYSTEM_STORE_LOCAL_MACHINE: u32 =
        CERT_SYSTEM_STORE_LOCAL_MACHINE_ID << CERT_SYSTEM_STORE_LOCATION_SHIFT;
    let store = unsafe {
        CertOpenStore(
            CERT_STORE_PROV_SYSTEM_W as PCSTR,
            X509_ASN_ENCODING,
            0,
            CERT_SYSTEM_STORE_LOCAL_MACHINE,
            store_name_w.as_ptr() as *const std::ffi::c_void,
        )
    };
    if store.is_null() {
        return Err(format!(
            "CertOpenStore(LocalMachine\\{}) failed: Win32 error {}",
            store_name,
            unsafe { GetLastError() }
        ));
    }
    let _store_guard = CertStoreGuard(store);

    // Encode the subject name we're searching for once; CertFindCertificateInStore
    // uses CRYPT_INTEGER_BLOB-shaped pvFindPara for CERT_FIND_SUBJECT_NAME.
    let subject_c = cstring(cert_subject);
    let mut size: u32 = 0;
    if unsafe {
        CertStrToNameA(
            X509_ASN_ENCODING,
            subject_c.as_ptr() as PCSTR,
            CERT_X500_NAME_STR,
            null(),
            null_mut(),
            &mut size,
            null_mut(),
        )
    } == 0
    {
        return Err(format!(
            "CertStrToNameA(sizing) failed: Win32 error {}",
            unsafe { GetLastError() }
        ));
    }
    let mut subject_buf = vec![0u8; size as usize];
    if unsafe {
        CertStrToNameA(
            X509_ASN_ENCODING,
            subject_c.as_ptr() as PCSTR,
            CERT_X500_NAME_STR,
            null(),
            subject_buf.as_mut_ptr(),
            &mut size,
            null_mut(),
        )
    } == 0
    {
        return Err(format!("CertStrToNameA failed: Win32 error {}", unsafe {
            GetLastError()
        }));
    }
    let subject_blob = CRYPT_INTEGER_BLOB {
        cbData: size,
        pbData: subject_buf.as_mut_ptr(),
    };

    let mut deleted = 0usize;
    loop {
        let cert = unsafe {
            CertFindCertificateInStore(
                store,
                X509_ASN_ENCODING,
                0,
                CERT_FIND_SUBJECT_NAME,
                &subject_blob as *const _ as *const std::ffi::c_void,
                null(),
            )
        };
        if cert.is_null() {
            break;
        }
        if unsafe { CertDeleteCertificateFromStore(cert) } == 0 {
            // Stop on a deletion failure rather than infinite-looping
            // on the same cert.
            return Err(format!(
                "CertDeleteCertificateFromStore(LocalMachine\\{}) failed: Win32 error {}",
                store_name,
                unsafe { GetLastError() }
            ));
        }
        deleted += 1;
    }
    Ok(deleted)
}

/// Common suffix used by every Aokie-issued self-signed cert (see
/// `create_and_sign_cat`'s subject format string). The cert subject CN
/// is `<hardware_id> (aokie autogenerated)` — matching on the suffix
/// catches every Aokie cert regardless of which dongle drove the
/// install, while leaving any other LocalMachine cert untouched.
const AOKIE_CERT_SUBJECT_SUFFIX: &str = " (aokie autogenerated)";

/// One Aokie-issued self-signed cert located in a LocalMachine store.
/// Returned by [`enumerate_aokie_certs`] for the UI to display before
/// the operator triggers removal — gives them a chance to confirm
/// what's about to disappear.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AokieCertInfo {
    /// Which LocalMachine store the cert lives in (`Root` or
    /// `TrustedPublisher`).
    pub store_name: String,
    /// Display form of the cert's subject CN — `<hardware_id> (aokie
    /// autogenerated)`.
    pub subject: String,
    /// SHA-1 thumbprint as uppercase hex. Stable identifier across
    /// the two stores when the same cert ends up in both.
    pub thumbprint: String,
}

/// List every Aokie-signed cert in `store_name` (under LocalMachine).
/// Read-only — works without admin elevation, and is the call the
/// frontend uses to populate "what would Remove actually delete?"
/// before the operator commits to the UAC prompt.
pub fn enumerate_aokie_certs(store_name: &str) -> Result<Vec<AokieCertInfo>, String> {
    let store_name_w = wide_null(store_name);
    const CERT_SYSTEM_STORE_LOCAL_MACHINE: u32 =
        CERT_SYSTEM_STORE_LOCAL_MACHINE_ID << CERT_SYSTEM_STORE_LOCATION_SHIFT;
    let store = unsafe {
        CertOpenStore(
            CERT_STORE_PROV_SYSTEM_W as PCSTR,
            X509_ASN_ENCODING,
            0,
            CERT_SYSTEM_STORE_LOCAL_MACHINE,
            store_name_w.as_ptr() as *const std::ffi::c_void,
        )
    };
    if store.is_null() {
        return Err(format!(
            "CertOpenStore(LocalMachine\\{}) failed: Win32 error {}",
            store_name,
            unsafe { GetLastError() }
        ));
    }
    let _store_guard = CertStoreGuard(store);

    let mut out: Vec<AokieCertInfo> = Vec::new();
    let mut cursor: *mut CERT_CONTEXT = null_mut();
    loop {
        cursor = unsafe { CertEnumCertificatesInStore(store, cursor) };
        if cursor.is_null() {
            break;
        }
        let subject = match cert_subject_display(cursor) {
            Some(s) => s,
            None => continue,
        };
        if !subject.ends_with(AOKIE_CERT_SUBJECT_SUFFIX) {
            continue;
        }
        let thumbprint = cert_sha1_thumbprint(cursor).unwrap_or_default();
        out.push(AokieCertInfo {
            store_name: store_name.to_string(),
            subject,
            thumbprint,
        });
    }
    Ok(out)
}

/// Delete every Aokie-signed cert from `store_name` (under LocalMachine)
/// and return the count removed. Requires admin elevation — the
/// frontend triggers this through the helper exe, never directly from
/// the app process. Companion to [`enumerate_aokie_certs`].
pub fn remove_all_aokie_certs(store_name: &str) -> Result<usize, String> {
    let store_name_w = wide_null(store_name);
    const CERT_SYSTEM_STORE_LOCAL_MACHINE: u32 =
        CERT_SYSTEM_STORE_LOCAL_MACHINE_ID << CERT_SYSTEM_STORE_LOCATION_SHIFT;
    let store = unsafe {
        CertOpenStore(
            CERT_STORE_PROV_SYSTEM_W as PCSTR,
            X509_ASN_ENCODING,
            0,
            CERT_SYSTEM_STORE_LOCAL_MACHINE,
            store_name_w.as_ptr() as *const std::ffi::c_void,
        )
    };
    if store.is_null() {
        return Err(format!(
            "CertOpenStore(LocalMachine\\{}) failed: Win32 error {}",
            store_name,
            unsafe { GetLastError() }
        ));
    }
    let _store_guard = CertStoreGuard(store);

    // Two-pass: first enumerate and duplicate every matching context,
    // then delete the duplicates. Single-pass enumeration + delete is
    // unsafe because CertDeleteCertificateFromStore frees the context
    // we'd pass back to CertEnumCertificatesInStore for the next
    // iteration cursor, and the Win32 docs explicitly warn against
    // that pattern. CertDuplicateCertificateContext gives us an owned
    // copy whose lifetime survives the enumeration walk.
    let mut to_delete: Vec<*mut CERT_CONTEXT> = Vec::new();
    let mut cursor: *mut CERT_CONTEXT = null_mut();
    loop {
        cursor = unsafe { CertEnumCertificatesInStore(store, cursor) };
        if cursor.is_null() {
            break;
        }
        let Some(subject) = cert_subject_display(cursor) else {
            continue;
        };
        if !subject.ends_with(AOKIE_CERT_SUBJECT_SUFFIX) {
            continue;
        }
        let dup = unsafe { CertDuplicateCertificateContext(cursor) };
        if !dup.is_null() {
            to_delete.push(dup);
        }
    }

    let mut deleted = 0usize;
    for ctx in to_delete {
        // CertDeleteCertificateFromStore frees the context on success
        // and on failure both — the duplicate handle is consumed either
        // way, which is why we don't free it ourselves on the error
        // branch. Stop on first failure: a partial deletion is fine to
        // report, but we want the operator to see the underlying error
        // instead of looping past it.
        if unsafe { CertDeleteCertificateFromStore(ctx) } == 0 {
            return Err(format!(
                "CertDeleteCertificateFromStore(LocalMachine\\{}) failed after {} \
                 deletion(s): Win32 error {}",
                store_name,
                deleted,
                unsafe { GetLastError() }
            ));
        }
        deleted += 1;
    }
    Ok(deleted)
}

/// Read the subject CN of `cert` in display form. Returns None if the
/// Win32 call fails or yields an empty string. Two passes through
/// `CertGetNameStringA`: first to size the buffer, second to fill it.
fn cert_subject_display(cert: *mut CERT_CONTEXT) -> Option<String> {
    let len = unsafe {
        CertGetNameStringA(
            cert,
            CERT_NAME_SIMPLE_DISPLAY_TYPE,
            0,
            null_mut(),
            null_mut(),
            0,
        )
    };
    if len <= 1 {
        return None;
    }
    let mut buf = vec![0u8; len as usize];
    let written = unsafe {
        CertGetNameStringA(
            cert,
            CERT_NAME_SIMPLE_DISPLAY_TYPE,
            0,
            null_mut(),
            buf.as_mut_ptr(),
            len,
        )
    };
    if written <= 1 {
        return None;
    }
    // Strip the trailing NUL CertGetNameStringA writes.
    if let Some(&0) = buf.last() {
        buf.pop();
    }
    String::from_utf8(buf).ok().filter(|s| !s.is_empty())
}

/// Read the SHA-1 thumbprint of `cert` as uppercase hex. Mirrors what
/// `certmgr.msc` shows in the cert's "Thumbprint" property and lets the
/// UI distinguish duplicate-subject certs that share a CN but differ
/// by issuer/keypair.
fn cert_sha1_thumbprint(cert: *mut CERT_CONTEXT) -> Option<String> {
    let mut size: u32 = 0;
    if unsafe {
        CertGetCertificateContextProperty(cert, CERT_SHA1_HASH_PROP_ID, null_mut(), &mut size)
    } == 0
        || size == 0
    {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    if unsafe {
        CertGetCertificateContextProperty(
            cert,
            CERT_SHA1_HASH_PROP_ID,
            buf.as_mut_ptr() as *mut std::ffi::c_void,
            &mut size,
        )
    } == 0
    {
        return None;
    }
    let mut hex = String::with_capacity(size as usize * 2);
    for byte in &buf[..size as usize] {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{:02X}", byte);
    }
    Some(hex)
}

fn create_self_signed_cert(cert_subject: &str) -> Result<CertContextOwned, String> {
    // EKU = code signing only.
    let code_signing_oid = cstring(SZ_OID_PKIX_KP_CODE_SIGNING);
    let mut eku_array: [PSTR; 1] = [code_signing_oid.as_ptr() as PSTR];
    let eku = CertEnhkeyUsage {
        c_usage_identifier: 1,
        rgpsz_usage_identifier: eku_array.as_mut_ptr(),
    };
    let oid_eku = cstring(SZ_OID_ENHANCED_KEY_USAGE);

    let mut size: u32 = 0;
    let ok = unsafe {
        CryptEncodeObjectEx(
            X509_ASN_ENCODING,
            X509_ENHANCED_KEY_USAGE,
            &eku as *const _ as *const std::ffi::c_void,
            0,
            null(),
            null_mut(),
            &mut size,
        )
    };
    if ok == 0 || size == 0 {
        return Err(format!(
            "CryptEncodeObjectEx(eku, sizing) failed: Win32 error {}",
            unsafe { GetLastError() }
        ));
    }
    let mut eku_buf = vec![0u8; size as usize];
    let ok = unsafe {
        CryptEncodeObjectEx(
            X509_ASN_ENCODING,
            X509_ENHANCED_KEY_USAGE,
            &eku as *const _ as *const std::ffi::c_void,
            0,
            null(),
            eku_buf.as_mut_ptr() as *mut std::ffi::c_void,
            &mut size,
        )
    };
    if ok == 0 {
        return Err(format!(
            "CryptEncodeObjectEx(eku) failed: Win32 error {}",
            unsafe { GetLastError() }
        ));
    }
    eku_buf.truncate(size as usize);

    let mut extensions = [CERT_EXTENSION {
        pszObjId: oid_eku.as_ptr() as PSTR,
        fCritical: TRUE,
        Value: CRYPT_DATA_BLOB {
            cbData: eku_buf.len() as u32,
            pbData: eku_buf.as_mut_ptr(),
        },
    }];
    let cert_extensions = CERT_EXTENSIONS {
        cExtension: extensions.len() as u32,
        rgExtension: extensions.as_mut_ptr(),
    };

    // Acquire (or create) the keypair container in the machine keyset.
    let key_container_w = wide_null(KEY_CONTAINER_NAME);
    let mut hcsp: usize = 0;
    if unsafe {
        CryptAcquireContextW(
            &mut hcsp,
            key_container_w.as_ptr(),
            null(),
            PROV_RSA_FULL,
            CRYPT_MACHINE_KEYSET | CRYPT_SILENT,
        )
    } == 0
    {
        let err = unsafe { GetLastError() } as i32;
        if err == NTE_BAD_KEYSET || err == NTE_KEYSET_ENTRY_BAD {
            if unsafe {
                CryptAcquireContextW(
                    &mut hcsp,
                    key_container_w.as_ptr(),
                    null(),
                    PROV_RSA_FULL,
                    CRYPT_NEWKEYSET | CRYPT_MACHINE_KEYSET | CRYPT_SILENT,
                )
            } == 0
            {
                return Err(format!(
                    "CryptAcquireContext(NEWKEYSET) failed: Win32 error {}",
                    unsafe { GetLastError() }
                ));
            }
        } else {
            return Err(format!(
                "CryptAcquireContext(machine keyset) failed: Win32 error {}",
                err
            ));
        }
    }
    let _hprov_guard = HCryptProvGuard(hcsp);

    // Generate RSA-2048 keypair. Key size goes in the high 16 bits of
    // the flags param. 2048 is what libwdi uses — sufficient for a
    // self-signed cat-signing cert that's only trusted long enough for
    // Windows to verify the cat at install time, and ~10x faster to
    // generate than 4096 (matters during interactive UAC flows).
    let mut hkey: usize = 0;
    let key_size_flag = 2048u32 << 16;
    if unsafe { CryptGenKey(hcsp, AT_SIGNATURE, key_size_flag, &mut hkey) } == 0 {
        return Err(format!(
            "CryptGenKey(AT_SIGNATURE, 2048) failed: Win32 error {}",
            unsafe { GetLastError() }
        ));
    }
    let _hkey_guard = HCryptKeyGuard(hkey);

    // Encode the X.500 subject blob.
    let subject_c = cstring(cert_subject);
    let mut size: u32 = 0;
    if unsafe {
        CertStrToNameA(
            X509_ASN_ENCODING,
            subject_c.as_ptr() as PCSTR,
            CERT_X500_NAME_STR,
            null(),
            null_mut(),
            &mut size,
            null_mut(),
        )
    } == 0
    {
        return Err(format!(
            "CertStrToNameA(sizing) failed: Win32 error {}",
            unsafe { GetLastError() }
        ));
    }
    let mut subject_buf = vec![0u8; size as usize];
    if unsafe {
        CertStrToNameA(
            X509_ASN_ENCODING,
            subject_c.as_ptr() as PCSTR,
            CERT_X500_NAME_STR,
            null(),
            subject_buf.as_mut_ptr(),
            &mut size,
            null_mut(),
        )
    } == 0
    {
        return Err(format!("CertStrToNameA failed: Win32 error {}", unsafe {
            GetLastError()
        }));
    }
    let subject_blob = CERT_NAME_BLOB {
        cbData: size,
        pbData: subject_buf.as_mut_ptr(),
    };

    let mut key_container_w_mut = wide_null(KEY_CONTAINER_NAME);
    let key_prov_info = CRYPT_KEY_PROV_INFO {
        pwszContainerName: key_container_w_mut.as_mut_ptr(),
        pwszProvName: null_mut(),
        dwProvType: PROV_RSA_FULL,
        dwFlags: CRYPT_MACHINE_KEYSET,
        cProvParam: 0,
        rgProvParam: null_mut(),
        dwKeySpec: AT_SIGNATURE,
    };

    let sha256_oid = cstring(SZ_OID_RSA_SHA256RSA);
    let signature_algorithm = SignatureAlgorithmLocal {
        psz_obj_id: sha256_oid.as_ptr() as PSTR,
        parameters: CryptObjectIdBlob {
            cb_data: 0,
            pb_data: null_mut(),
        },
    };

    // pStartTime=NULL — match libwdi (`pki.c` line 840). An earlier
    // revision passed `now - 10min` as a clock-skew guard, which
    // coincided with persistent CERT_E_CHAINING (0x800B010A) from
    // SignerSignEx's chain builder. libwdi's NULL has been the
    // working pattern on every Windows version, so we match it
    // before chasing more exotic causes.
    let now = chrono::Local::now();
    let end = now + chrono::Duration::days(5 * 365);
    let end_st = systemtime_from_chrono(end);

    let cert = unsafe {
        CertCreateSelfSignCertificate(
            0,
            &subject_blob,
            0,
            &key_prov_info,
            &signature_algorithm as *const _ as *const _,
            null(),
            &end_st,
            &cert_extensions,
        )
    };
    if cert.is_null() {
        return Err(format!(
            "CertCreateSelfSignCertificate failed: Win32 error {}",
            unsafe { GetLastError() }
        ));
    }
    Ok(CertContextOwned(cert as *mut CERT_CONTEXT))
}

fn add_cert_to_store(cert: *mut CERT_CONTEXT, store_name: &str) -> Result<(), String> {
    let store_name_w = wide_null(store_name);

    // CERT_STORE_PROV_SYSTEM_W (10) matches libwdi's CERT_STORE_PROV_SYSTEM_A
    // (9) — same SYSTEM store provider, just UTF-16 vs ASCII store-name
    // encoding. Earlier this code used CERT_STORE_PROV_SYSTEM_REGISTRY_W
    // (13), which is a different provider (registry-only) and bypasses
    // some of the chain-engine lookup machinery: a cert added there is
    // present in `certmgr.msc` but invisible to SignerSignEx's chain
    // builder, which then fails with CERT_E_CHAINING (0x800B010A).
    const CERT_SYSTEM_STORE_LOCAL_MACHINE: u32 =
        CERT_SYSTEM_STORE_LOCAL_MACHINE_ID << CERT_SYSTEM_STORE_LOCATION_SHIFT;
    let store = unsafe {
        CertOpenStore(
            CERT_STORE_PROV_SYSTEM_W as PCSTR,
            X509_ASN_ENCODING,
            0,
            CERT_SYSTEM_STORE_LOCAL_MACHINE,
            store_name_w.as_ptr() as *const std::ffi::c_void,
        )
    };
    if store.is_null() {
        return Err(format!(
            "CertOpenStore(LocalMachine\\{}) failed: Win32 error {}",
            store_name,
            unsafe { GetLastError() }
        ));
    }
    let _store_guard = CertStoreGuard(store);

    // Friendly name so the cert shows up sensibly in certmgr.msc.
    let mut friendly: Vec<u16> = "Aokie".encode_utf16().chain(std::iter::once(0)).collect();
    let friendly_blob = CRYPT_DATA_BLOB {
        cbData: (friendly.len() * 2) as u32,
        pbData: friendly.as_mut_ptr() as *mut u8,
    };
    if unsafe {
        CertSetCertificateContextProperty(
            cert,
            CERT_FRIENDLY_NAME_PROP_ID,
            0,
            &friendly_blob as *const _ as *const std::ffi::c_void,
        )
    } == 0
    {
        return Err(format!(
            "CertSetCertificateContextProperty(friendly) failed: Win32 error {}",
            unsafe { GetLastError() }
        ));
    }

    if unsafe {
        CertAddCertificateContextToStore(store, cert, CERT_STORE_ADD_REPLACE_EXISTING, null_mut())
    } == 0
    {
        return Err(format!(
            "CertAddCertificateContextToStore(LocalMachine\\{}) failed: Win32 error {}",
            store_name,
            unsafe { GetLastError() }
        ));
    }

    Ok(())
}

fn sign_cat_with_cert(path: &Path, cert: *mut CERT_CONTEXT) -> Result<(), String> {
    let path_w = wide_null(&path.to_string_lossy());

    let signer_file_info = SIGNER_FILE_INFO {
        cbSize: size_of::<SIGNER_FILE_INFO>() as u32,
        pwszFileName: path_w.as_ptr(),
        hFile: null_mut(),
    };
    let mut subject_index: u32 = 0;
    let mut signer_subject_info: SIGNER_SUBJECT_INFO = unsafe { zeroed() };
    signer_subject_info.cbSize = size_of::<SIGNER_SUBJECT_INFO>() as u32;
    signer_subject_info.pdwIndex = &mut subject_index;
    signer_subject_info.dwSubjectChoice = SIGNER_SUBJECT_FILE;
    signer_subject_info.Anonymous = SIGNER_SUBJECT_INFO_0 {
        pSignerFileInfo: &signer_file_info as *const _ as *mut _,
    };

    let signer_cert_store_info = SIGNER_CERT_STORE_INFO {
        cbSize: size_of::<SIGNER_CERT_STORE_INFO>() as u32,
        pSigningCert: cert,
        dwCertPolicy: SIGNER_CERT_POLICY_CHAIN,
        hCertStore: null_mut(),
    };
    let mut signer_cert: SIGNER_CERT = unsafe { zeroed() };
    signer_cert.cbSize = size_of::<SIGNER_CERT>() as u32;
    signer_cert.dwCertChoice = SIGNER_CERT_STORE;
    signer_cert.Anonymous = SIGNER_CERT_0 {
        pCertStoreInfo: &signer_cert_store_info as *const _ as *mut _,
    };
    signer_cert.hwnd = null_mut();

    // Authenticode-conforming signed catalogs carry two authenticated
    // attributes: SPC_SP_OPUS_INFO (the signing-time opus, even if
    // empty) and SPC_STATEMENT_TYPE (the role of the signature). signtool
    // emits both for every code-signed file. Without them some
    // verifiers — and stricter code-integrity policies — reject the
    // signature as non-conforming. The blobs are pre-encoded ASN.1 DER
    // libwdi ships verbatim.
    let opus_oid = cstring(SPC_SP_OPUS_INFO_OBJID);
    let statement_oid = cstring(SPC_STATEMENT_TYPE_OBJID);
    let mut opus_blob = CRYPT_INTEGER_BLOB {
        cbData: SP_OPUS_INFO_DATA.len() as u32,
        pbData: SP_OPUS_INFO_DATA.as_ptr() as *mut u8,
    };
    let mut statement_blob = CRYPT_INTEGER_BLOB {
        cbData: STATEMENT_TYPE_DATA.len() as u32,
        pbData: STATEMENT_TYPE_DATA.as_ptr() as *mut u8,
    };
    let mut auth_attrs = [
        CRYPT_ATTRIBUTE {
            pszObjId: opus_oid.as_ptr() as PSTR,
            cValue: 1,
            rgValue: &mut opus_blob,
        },
        CRYPT_ATTRIBUTE {
            pszObjId: statement_oid.as_ptr() as PSTR,
            cValue: 1,
            rgValue: &mut statement_blob,
        },
    ];
    let mut authenticated = CRYPT_ATTRIBUTES {
        cAttr: auth_attrs.len() as u32,
        rgAttr: auth_attrs.as_mut_ptr(),
    };

    let mut signer_signature_info: SIGNER_SIGNATURE_INFO = unsafe { zeroed() };
    signer_signature_info.cbSize = size_of::<SIGNER_SIGNATURE_INFO>() as u32;
    signer_signature_info.algidHash = CALG_SHA_256;
    signer_signature_info.dwAttrChoice = SIGNER_NO_ATTR;
    signer_signature_info.Anonymous = SIGNER_SIGNATURE_INFO_0 {
        pAttrAuthcode: null_mut(),
    };
    signer_signature_info.psAuthenticated = &mut authenticated;
    signer_signature_info.psUnauthenticated = null_mut();

    let mut psigner_context: *mut windows_sys::Win32::Security::Cryptography::SIGNER_CONTEXT =
        null_mut();
    let hr = unsafe {
        SignerSignEx(
            0,
            &signer_subject_info,
            &signer_cert,
            &signer_signature_info,
            null(),
            null(),
            null(),
            null(),
            &mut psigner_context,
        )
    };
    if hr != 0 {
        return Err(format!("SignerSignEx failed: HRESULT 0x{:08x}", hr as u32));
    }
    if !psigner_context.is_null() {
        unsafe {
            SignerFreeSignerContext(psigner_context);
        }
    }
    Ok(())
}

fn destroy_signing_keypair() -> Result<(), String> {
    let key_container_w = wide_null(KEY_CONTAINER_NAME);
    let mut hcsp: usize = 0;
    if unsafe {
        CryptAcquireContextW(
            &mut hcsp,
            key_container_w.as_ptr(),
            null(),
            PROV_RSA_FULL,
            CRYPT_MACHINE_KEYSET | CRYPT_SILENT | CRYPT_DELETEKEYSET,
        )
    } == 0
    {
        return Err(format!(
            "CryptAcquireContext(DELETEKEYSET) failed: Win32 error {}",
            unsafe { GetLastError() }
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

fn wide_z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_lower_z(s: &str) -> Vec<u16> {
    s.to_lowercase()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
}

fn cstring(s: &str) -> CString {
    CString::new(s).expect("OID strings have no internal NULs")
}

fn cert_free(cert: *mut CERT_CONTEXT) {
    if !cert.is_null() {
        unsafe {
            CertFreeCertificateContext(cert);
        }
    }
}

fn systemtime_from_chrono(
    t: chrono::DateTime<chrono::Local>,
) -> windows_sys::Win32::Foundation::SYSTEMTIME {
    use chrono::Datelike;
    use chrono::Timelike;
    windows_sys::Win32::Foundation::SYSTEMTIME {
        wYear: t.year() as u16,
        wMonth: t.month() as u16,
        wDayOfWeek: t.weekday().num_days_from_sunday() as u16,
        wDay: t.day() as u16,
        wHour: t.hour() as u16,
        wMinute: t.minute() as u16,
        wSecond: t.second() as u16,
        wMilliseconds: 0,
    }
}

struct HCryptProvGuard(usize);
impl Drop for HCryptProvGuard {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe {
                CryptReleaseContext(self.0, 0);
            }
        }
    }
}

struct HCryptKeyGuard(usize);
impl Drop for HCryptKeyGuard {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe {
                CryptDestroyKey(self.0);
            }
        }
    }
}

struct CatGuard(HANDLE);
impl Drop for CatGuard {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE && !self.0.is_null() {
            unsafe {
                CryptCATClose(self.0);
            }
        }
    }
}

struct FileGuard(HANDLE);
impl Drop for FileGuard {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE && !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

struct CertStoreGuard(*mut std::ffi::c_void);
impl Drop for CertStoreGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CertCloseStore(self.0, 0);
            }
        }
    }
}

struct CertContextOwned(*mut CERT_CONTEXT);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_cat_path_from_inf() {
        let p = std::path::PathBuf::from(r"C:\foo\bar.inf");
        assert_eq!(
            derive_cat_path(&p).unwrap(),
            std::path::PathBuf::from(r"C:\foo\bar.cat")
        );
    }

    #[test]
    fn rejects_non_inf_path() {
        let p = std::path::PathBuf::from(r"C:\foo\bar.txt");
        assert!(derive_cat_path(&p).is_err());
    }
}
