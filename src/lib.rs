//
// Copyright (c) 2022 ZettaScale Technology
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
//

use async_trait::async_trait;
use log::{debug, warn};
use std::convert::TryInto;
use std::io::prelude::*;
use std::path::PathBuf;
use std::{fs::DirBuilder, sync::Arc};
use tempfile::tempfile_in;
use zenoh::prelude::r#async::AsyncResolve;
use zenoh::prelude::*;
use zenoh::time::new_reception_timestamp;
use zenoh::Result as ZResult;
use zenoh_backend_traits::{
    config::StorageConfig, config::VolumeConfig, CreateVolume, Query, Storage,
    StorageInsertionResult, Volume,
};
use zenoh_core::{bail, zerror};
use zenoh_util::zenoh_home;

mod data_info_mgt;
mod files_mgt;
use files_mgt::*;

/// The environement variable used to configure the root of all storages managed by this FileSystemBackend.
pub const SCOPE_ENV_VAR: &str = "ZBACKEND_FS_ROOT";

/// The default root (whithin zenoh's home directory) if the ZBACKEND_FS_ROOT environment variable is not specified.
pub const DEFAULT_ROOT_DIR: &str = "zbackend_fs";

// Properies used by the Backend
//  - None

// Properies used by the Storage
pub const PROP_STORAGE_READ_ONLY: &str = "read_only";
pub const PROP_STORAGE_DIR: &str = "dir";
pub const PROP_STORAGE_ON_CLOSURE: &str = "on_closure";
pub const PROP_STORAGE_FOLLOW_LINK: &str = "follow_links";
pub const PROP_STORAGE_KEEP_MIME: &str = "keep_mime_types";

const GIT_VERSION: &str = git_version::git_version!(prefix = "v", cargo_prefix = "v");
lazy_static::lazy_static!(
    static ref LONG_VERSION: String = format!("{} built with {}", GIT_VERSION, env!("RUSTC_VERSION"));
);

#[allow(dead_code)]
/// Serves as typecheck for the create_backend function, ensuring it has the expected signature
const CREATE_VOLUME_TYPECHECK: CreateVolume = create_volume;

#[no_mangle]
pub fn create_volume(_unused: VolumeConfig) -> ZResult<Box<dyn Volume>> {
    // For some reasons env_logger is sometime not active in a loaded library.
    // Try to activate it here, ignoring failures.
    let _ = env_logger::try_init();
    debug!("FileSystem backend {}", LONG_VERSION.as_str());

    let root_path = if let Some(dir) = std::env::var_os(SCOPE_ENV_VAR) {
        PathBuf::from(dir)
    } else {
        let mut dir = PathBuf::from(zenoh_home());
        dir.push(DEFAULT_ROOT_DIR);
        dir
    };
    if let Err(e) = std::fs::create_dir_all(&root_path) {
        bail!(
            r#"Failed to create directory ${{{}}}={}: {}"#,
            SCOPE_ENV_VAR,
            root_path.display(),
            e
        );
    }
    let root = match dunce::canonicalize(&root_path) {
        Ok(dir) => dir,
        Err(e) => bail!(
            r#"Invalid path for ${{{}}}={}: {}"#,
            SCOPE_ENV_VAR,
            root_path.display(),
            e
        ),
    };
    debug!("Using root dir: {}", root.display());

    let mut properties = zenoh::properties::Properties::default();
    properties.insert("root".into(), root.to_string_lossy().into());
    properties.insert("version".into(), LONG_VERSION.clone());

    let admin_status = properties
        .0
        .into_iter()
        .map(|(k, v)| (k, serde_json::Value::String(v)))
        .collect();
    Ok(Box::new(FileSystemBackend { admin_status, root }))
}

pub struct FileSystemBackend {
    admin_status: serde_json::Value,
    root: PathBuf,
}

fn extract_bool(
    from: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    default: bool,
) -> ZResult<bool> {
    match from.get(key) {
        Some(serde_json::Value::Bool(s)) => Ok(*s),
        None => Ok(default),
        _ => bail!(
            r#"Invalid value for File System Storage configuration: `{}` must be a boolean"#,
            key
        ),
    }
}

#[async_trait]
impl Volume for FileSystemBackend {
    fn get_admin_status(&self) -> serde_json::Value {
        self.admin_status.clone()
    }

    async fn create_storage(&mut self, mut config: StorageConfig) -> ZResult<Box<dyn Storage>> {
        let volume_cfg = match config.volume_cfg.as_object() {
            Some(v) => v,
            None => bail!("fs backed volumes require volume-specific configuration"),
        };

        let read_only = extract_bool(volume_cfg, PROP_STORAGE_READ_ONLY, false)?;
        let follow_links = extract_bool(volume_cfg, PROP_STORAGE_FOLLOW_LINK, false)?;
        let keep_mime = extract_bool(volume_cfg, PROP_STORAGE_KEEP_MIME, true)?;
        let on_closure = match config.volume_cfg.get(PROP_STORAGE_ON_CLOSURE) {
            Some(serde_json::Value::String(s)) if s == "delete_all" => OnClosure::DeleteAll,
            Some(serde_json::Value::String(s)) if s == "do_nothing" => OnClosure::DoNothing,
            None => OnClosure::DoNothing,
            Some(s) => {
                bail!(
                    r#"Unsupported value {:?} for `on_closure` property: must be either "delete_all" or "do_nothing". Default is "do_nothing""#,
                    s
                )
            }
        };

        let base_dir =
            if let Some(serde_json::Value::String(dir)) = config.volume_cfg.get(PROP_STORAGE_DIR) {
                let dir_path = PathBuf::from(dir.as_str());
                if dir_path.is_absolute() {
                    bail!(
                        r#"Invalid property "{}"="{}": the path must be relative"#,
                        PROP_STORAGE_DIR,
                        dir
                    );
                }
                if dir_path
                    .components()
                    .any(|c| c == std::path::Component::ParentDir)
                {
                    bail!(
                        r#"Invalid property "{}"="{}": the path must not contain any '..'"#,
                        PROP_STORAGE_DIR,
                        dir
                    );
                }

                // prepend base_dir with self.root
                let mut base_dir = self.root.clone();
                base_dir.push(dir_path);
                base_dir
            } else {
                bail!(
                    r#"Missing required property for File System Storage: "{}""#,
                    PROP_STORAGE_DIR
                )
            };

        // check if base_dir exists and is readable (and writeable if not "read_only" mode)
        let mut dir_builder = DirBuilder::new();
        dir_builder.recursive(true);
        let base_dir_path = PathBuf::from(&base_dir);
        if !base_dir_path.exists() {
            if let Err(err) = dir_builder.create(&base_dir) {
                bail!(
                    r#"Cannot create File System Storage on "dir"={:?} : {}"#,
                    base_dir,
                    err
                )
            }
        } else if !base_dir_path.is_dir() {
            bail!(
                r#"Cannot create File System Storage on "dir"={:?} : this is not a directory"#,
                base_dir
            )
        } else if let Err(err) = base_dir_path.read_dir() {
            bail!(
                r#"Cannot create File System Storage on "dir"={:?} : {}"#,
                base_dir,
                err
            )
        } else if !read_only {
            // try to write a random file
            let _ = tempfile_in(&base_dir)
                .map(|mut f| writeln!(f, "test"))
                .map_err(|err| {
                    zerror!(
                        r#"Cannot create writeable File System Storage on "dir"={:?} : {}"#,
                        base_dir,
                        err
                    )
                })?;
        }

        config
            .volume_cfg
            .as_object_mut()
            .unwrap()
            .insert("dir_full_path".into(), base_dir.to_string_lossy().into());

        log::debug!(
            "Storage on {} will store files in {}",
            config.key_expr,
            base_dir.display()
        );

        let files_mgr = FilesMgr::new(base_dir, follow_links, keep_mime, on_closure).await?;
        Ok(Box::new(FileSystemStorage {
            config,
            files_mgr,
            read_only,
        }))
    }

    fn incoming_data_interceptor(&self) -> Option<Arc<dyn Fn(Sample) -> Sample + Sync + Send>> {
        None
    }

    fn outgoing_data_interceptor(&self) -> Option<Arc<dyn Fn(Sample) -> Sample + Sync + Send>> {
        None
    }
}

struct FileSystemStorage {
    config: StorageConfig,
    files_mgr: FilesMgr,
    read_only: bool,
}

impl FileSystemStorage {
    async fn reply_with_matching_files(&self, query: &Query, path_expr: &str) {
        match path_expr.try_into() {
            Ok(ke) => {
                for zfile in self.files_mgr.matching_files(ke) {
                    let trimmed_zpath = get_trimmed_keyexpr(zfile.zpath.as_ref());
                    let trimmed_zfile = self.files_mgr.to_zfile(trimmed_zpath);
                    self.reply_with_file(query, &trimmed_zfile).await;
                }
            }
            Err(e) => log::error!("Couldn't convert `{}` to key expression: {}", path_expr, e),
        }
    }

    async fn reply_with_file(&self, query: &Query, zfile: &ZFile<'_>) {
        match self.files_mgr.read_file(zfile).await {
            Ok(Some((value, timestamp))) => {
                debug!(
                    "Replying to query on {} with file {:?}",
                    query.selector(),
                    zfile,
                );
                // if strip_prefix is set, prefix it back to the zenoh path of this ZFile
                let zpath = match &self.config.strip_prefix {
                    Some(prefix) => prefix.join(zfile.zpath.as_ref()).unwrap(),
                    None => zfile.zpath.as_ref().try_into().unwrap(),
                };
                if let Err(e) = query
                    .reply(Sample::new(zpath, value).with_timestamp(timestamp))
                    .res()
                    .await
                {
                    log::error!(
                        "Error replying to query on {} with file {}: {}",
                        query.selector(),
                        zfile,
                        e
                    );
                }
                debug!("Reply sent !!!!!");
            }
            Ok(None) => (), // file not found, do nothing
            Err(e) => warn!(
                "Replying to query on {} : failed to read file {} : {}",
                query.selector(),
                zfile,
                e
            ),
        }
    }
}

#[async_trait]
impl Storage for FileSystemStorage {
    fn get_admin_status(&self) -> serde_json::Value {
        self.config.to_json_value()
    }

    // When receiving a Sample (i.e. on PUT or DELETE operations)
    async fn on_sample(&mut self, sample: Sample) -> ZResult<StorageInsertionResult> {
        // if strip_prefix is set, strip it from the sample key_expr for this ZFile
        let zfile = match &self.config.strip_prefix {
            Some(prefix) => match sample.key_expr.strip_prefix(prefix).as_slice() {
                [ke] => self.files_mgr.to_zfile(ke.as_str()),
                _ => bail!(
                    "Received a Sample with keyexpr not starting with path_prefix '{}': '{}'",
                    prefix,
                    sample.key_expr
                ),
            },
            None => self.files_mgr.to_zfile(sample.key_expr.as_str()),
        };

        // get latest timestamp for this file (if referenced in data-info db or if exists on disk)
        // and drop incoming sample if older
        let sample_ts = sample.timestamp.unwrap_or_else(new_reception_timestamp);
        if let Some(old_ts) = self.files_mgr.get_timestamp(&zfile).await? {
            if sample_ts < old_ts {
                debug!(
                    "{} on {} dropped: out-of-date",
                    sample.kind, sample.key_expr
                );
                return Ok(StorageInsertionResult::Outdated);
            }
        }

        // Store or delete the sample depending the ChangeKind
        match sample.kind {
            SampleKind::Put => {
                if !self.read_only {
                    // write file
                    self.files_mgr
                        .write_file(
                            &zfile,
                            sample.value.payload,
                            &sample.value.encoding,
                            &sample_ts,
                        )
                        .await?;
                    Ok(StorageInsertionResult::Inserted)
                } else {
                    warn!(
                        "Received PUT for read-only Files System Storage on {:?} - ignored",
                        self.files_mgr.base_dir()
                    );
                    Err("Received update for read-only File System Storage".into())
                }
            }
            SampleKind::Delete => {
                if !self.read_only {
                    // delete file
                    self.files_mgr.delete_file(&zfile, &sample_ts).await?;
                    Ok(StorageInsertionResult::Deleted)
                } else {
                    warn!(
                        "Received DELETE for read-only Files System Storage on {:?} - ignored",
                        self.files_mgr.base_dir()
                    );
                    Err("Received update for read-only File System Storage".into())
                }
            }
        }
    }

    // When receiving a Query (i.e. on GET operations)
    async fn on_query(&mut self, query: Query) -> ZResult<()> {
        // get the query's Selector
        let selector = query.selector();

        // if strip_prefix is set, strip it from the Selector's keyexpr to get the list of sub-keyexpr
        // that will match the same stored keys than the selector, if those keys had the path_prefix.
        let sub_keyexpr = match &self.config.strip_prefix {
            Some(prefix) => {
                let vec = selector.key_expr.strip_prefix(prefix);
                if vec.is_empty() {
                    warn!("Received query on selector '{}', but the configured strip_prefix='{:?}' is not a prefix of this selector", selector, self.config.strip_prefix);
                }
                vec
            }
            None => vec![selector.key_expr.as_keyexpr()],
        };

        for ke in sub_keyexpr {
            if ke.contains('*') {
                self.reply_with_matching_files(&query, ke).await;
            } else {
                // path_expr correspond to 1 single file.
                // Convert it to ZFile and reply it.
                let zfile = self.files_mgr.to_zfile(ke);
                self.reply_with_file(&query, &zfile).await;
            }
        }

        Ok(())
    }

    async fn get_all_entries(&self) -> ZResult<Vec<(OwnedKeyExpr, zenoh::time::Timestamp)>> {
        let mut result = Vec::new();

        // get all files in the filesystem
        for zfile in self
            .files_mgr
            .matching_files(unsafe { keyexpr::from_str_unchecked("**") })
        {
            let trimmed_zpath = get_trimmed_keyexpr(zfile.zpath.as_ref());
            let trimmed_zfile = self.files_mgr.to_zfile(trimmed_zpath);
            match self.files_mgr.read_file(&trimmed_zfile).await {
                Ok(Some((_, timestamp))) => {
                    // if strip_prefix is set, prefix it back to the zenoh path of this ZFile
                    let zpath = match &self.config.strip_prefix {
                        Some(prefix) => prefix.join(zfile.zpath.as_ref()).unwrap(),
                        None => zfile.zpath.as_ref().try_into().unwrap(),
                    };
                    result.push((zpath, timestamp));
                }
                Ok(None) => (), // file not found, do nothing
                Err(e) => warn!(
                    "Getting all entries : failed to read file {} : {}",
                    zfile, e
                ),
            }
        }
        // get deleted files information from rocksdb
        for (zpath, ts) in self.files_mgr.get_deleted_entries().await {
            // if strip_prefix is set, prefix it back to the zenoh path of this ZFile
            let zpath = match &self.config.strip_prefix {
                Some(prefix) => prefix.join(&zpath).unwrap(),
                None => zpath.try_into().unwrap(),
            };
            result.push((zpath, ts));
        }
        Ok(result)
    }
}
