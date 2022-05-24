use console::style;
use futures::future::select_all;
use std::{
    borrow::Borrow,
    cmp,
    collections::HashSet,
    ffi::OsStr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use crate::cache::{load_cache, Cache};
use crate::common::*;
use crate::config::get_config_data;
use crate::constants::PARALLEL_LIMIT;
use crate::upload::storage::*;
use crate::upload::*;
use crate::utils::*;
use crate::validate::format::Metadata;

pub struct UploadArgs {
    pub assets_dir: String,
    pub config: String,
    pub keypair: Option<String>,
    pub rpc_url: Option<String>,
    pub cache: String,
    pub interrupted: Arc<AtomicBool>,
}

pub struct AssetType {
    pub image: Vec<usize>,
    pub metadata: Vec<usize>,
    pub animation: Vec<usize>,
}

pub async fn process_upload(args: UploadArgs) -> Result<()> {
    let sugar_config = sugar_setup(args.keypair, args.rpc_url)?;
    let config_data = get_config_data(&args.config)?;

    // loading assets
    println!(
        "{} {}Loading assets",
        style("[1/4]").bold().dim(),
        ASSETS_EMOJI
    );

    let pb = spinner_with_style();
    pb.enable_steady_tick(120);
    pb.set_message("Reading files...");

    let asset_pairs = get_asset_pairs(&args.assets_dir)?;
    // creates/loads the cache
    let mut cache = load_cache(&args.cache, true)?;

    // list of indices to upload
    // 0: image
    // 1: metadata
    let mut indices = AssetType {
        image: Vec::new(),
        metadata: Vec::new(),
        animation: Vec::new(),
    };

    for (index, pair) in &asset_pairs {
        match cache.items.0.get_mut(&index.to_string()) {
            Some(item) => {
                // determining animation condition
                let animation_conditon =
                    if item.animation_hash.is_some() && item.animation_link.as_ref().is_some() {
                        !item.animation_hash.eq(&pair.animation_hash)
                            || item.animation_link.as_ref().unwrap().is_empty()
                    } else {
                        false
                    };

                // has the image file changed?
                if !&item.image_hash.eq(&pair.image_hash) || item.image_link.is_empty() {
                    // we replace the entire item to trigger the image and metadata upload
                    let item_clone = item.clone();
                    cache
                        .items
                        .0
                        .insert(index.to_string(), pair.clone().into_cache_item());
                    // we need to upload both image/metadata
                    indices.image.push(*index);
                    indices.metadata.push(*index);

                    if item_clone.animation_hash.is_some() || item_clone.animation_link.is_some() {
                        indices.animation.push(*index);
                    }
                } else if animation_conditon {
                    // we replace the entire item to trigger the image and metadata upload
                    cache
                        .items
                        .0
                        .insert(index.to_string(), pair.clone().into_cache_item());
                    // we need to upload both image/metadata
                    indices.animation.push(*index);
                    indices.image.push(*index);
                    indices.metadata.push(*index);
                } else if !item.metadata_hash.eq(&pair.metadata_hash)
                    || item.metadata_link.is_empty()
                {
                    // triggers the metadata upload
                    item.metadata_hash = pair.metadata_hash.clone();
                    item.metadata_link = String::new();
                    item.on_chain = false;
                    // we need to upload metadata only
                    indices.metadata.push(*index);
                }
            }
            None => {
                cache
                    .items
                    .0
                    .insert(index.to_string(), pair.clone().into_cache_item());
                // we need to upload both image/metadata
                indices.image.push(*index);
                indices.metadata.push(*index);

                if pair.animation_hash.clone().is_some() {
                    indices.animation.push(*index);
                };
            }
        }
        // sanity check: verifies that both symbol and seller-fee-basis-points are the
        // same as the ones in the config file
        let f = File::open(Path::new(&pair.metadata))?;
        match serde_json::from_reader(f) {
            Ok(metadata) => {
                let metadata: Metadata = metadata;
                // symbol check
                if config_data.symbol.ne(&metadata.symbol) {
                    return Err(UploadError::MismatchValue(
                        "symbol".to_string(),
                        pair.metadata.clone(),
                        config_data.symbol,
                        metadata.symbol,
                    )
                    .into());
                }
                // seller-fee-basis-points check
                if config_data.seller_fee_basis_points != metadata.seller_fee_basis_points {
                    return Err(UploadError::MismatchValue(
                        "seller_fee_basis_points".to_string(),
                        pair.metadata.clone(),
                        config_data.seller_fee_basis_points.to_string(),
                        metadata.seller_fee_basis_points.to_string(),
                    )
                    .into());
                }
            }
            Err(err) => {
                let error = anyhow!("Error parsing metadata ({}): {}", pair.metadata, err);
                error!("{:?}", error);
                return Err(error);
            }
        }
    }

    pb.finish_and_clear();

    println!(
        "Found {} image/metadata pair(s), uploading files:",
        asset_pairs.len()
    );
    println!("+--------------------+");
    println!("| images    | {:>6} |", indices.image.len());
    println!("| metadata  | {:>6} |", indices.metadata.len());
    if !indices.animation.is_empty() {
        println!("| animation | {:>6} |", indices.animation.len());
    }
    println!("+--------------------+");

    // this should never happen, since every time we update the image file we
    // need to update the metadata
    if indices.image.len() > indices.metadata.len() {
        return Err(anyhow!(format!(
            "There are more image files ({}) to upload than metadata ({})",
            indices.image.len(),
            indices.metadata.len(),
        )));
    }

    let need_upload =
        !indices.image.is_empty() || !indices.metadata.is_empty() || !indices.animation.is_empty();

    // ready to upload data

    let mut errors = Vec::new();

    if need_upload {
        println!(
            "\n{} {}Initializing upload",
            if !indices.animation.is_empty() {
                style("[2/5]").bold().dim()
            } else {
                style("[2/4]").bold().dim()
            },
            COMPUTER_EMOJI
        );

        let pb = spinner_with_style();
        pb.set_message("Connecting...");

        let storage = storage::initialize(&sugar_config, &config_data).await?;

        pb.finish_with_message("Connected");

        storage
            .prepare(
                &sugar_config,
                &asset_pairs,
                vec![
                    (DataType::Media, &indices.0),
                    (DataType::Metadata, &indices.1),
                ],
            )
            .await?;

        // clear the interruption handler value ahead of the upload
        args.interrupted.store(false, Ordering::SeqCst);

        println!(
            "\n{} {}Uploading image files {}",
            if !indices.animation.is_empty() {
                style("[3/5]").bold().dim()
            } else {
                style("[3/4]").bold().dim()
            },
            UPLOAD_EMOJI,
            if indices.image.is_empty() {
                "(skipping)"
            } else {
                ""
            }
        );

        if !indices.image.is_empty() {
            errors.extend(
                handler
                    .upload_data(
                        &sugar_config,
                        &asset_pairs,
                        &mut cache,
                        &indices.image,
                        DataType::Image,
                        args.interrupted.clone(),
                    )
                    .await?,
            );

            // updates the list of metadata indices since the image upload
            // might fail - removes any index that the image upload failed
            if !indices.metadata.is_empty() {
                for index in indices.image {
                    let item = cache.items.0.get(&index.to_string()).unwrap();

                    if item.image_link.is_empty() {
                        // no image link, not ready for metadata upload
                        indices.metadata.retain(|&x| x != index);
                    }
                }
            }
        }

        if !indices.animation.is_empty() {
            println!(
                "\n{} {}Uploading animation files {}",
                style("[4/5]").bold().dim(),
                UPLOAD_EMOJI,
                if indices.animation.is_empty() {
                    "(skipping)"
                } else {
                    ""
                }
            );
        }

        if !indices.animation.is_empty() {
            errors.extend(
                upload_data(
                    &asset_pairs,
                    &mut cache,
                    &indices.0,
                    DataType::Media,
                    storage.borrow(),
                    args.interrupted.clone(),
                )
                .await?,
            );

            // updates the list of metadata indices since the image upload
            // might fail - removes any index that the image upload failed
            if !indices.metadata.is_empty() {
                for index in indices.animation.clone() {
                    let item = cache.items.0.get(&index.to_string()).unwrap();

                    if item.animation_link.as_ref().unwrap().is_empty() {
                        // no image link, not ready for metadata upload
                        indices.metadata.retain(|&x| x != index);
                    }
                }
            }
        }

        println!(
            "\n{} {}Uploading metadata files {}",
            if !indices.animation.is_empty() {
                style("[5/5]").bold().dim()
            } else {
                style("[4/4]").bold().dim()
            },
            UPLOAD_EMOJI,
            if indices.metadata.is_empty() {
                "(skipping)"
            } else {
                ""
            }
        );

        if !indices.metadata.is_empty() {
            errors.extend(
                upload_data(
                    &asset_pairs,
                    &mut cache,
                    &indices.1,
                    DataType::Metadata,
                    storage.borrow(),
                    args.interrupted.clone(),
                )
                .await?,
            );
        }
    } else {
        println!("\n....no files need uploading, skipping remaining steps.");
    }

    // sanity check

    cache.items.0.sort_keys();
    cache.sync_file()?;

    let mut count = 0;

    for (_index, item) in cache.items.0 {
        let has_animation = if let Some(animation_link) = item.animation_link {
            animation_link.is_empty()
        } else {
            false
        };

        if !(item.image_link.is_empty() || item.metadata_link.is_empty() || has_animation) {
            count += 1;
        }
    }

    println!(
        "\n{}",
        if !indices.animation.is_empty() {
            style(format!(
                "{}/{} image/animation/metadata pair(s) uploaded.",
                count,
                asset_pairs.len()
            ))
            .bold()
        } else {
            style(format!(
                "{}/{} image/metadata pair(s) uploaded.",
                count,
                asset_pairs.len()
            ))
            .bold()
        }
    );

    if count != asset_pairs.len() {
        let message = if !errors.is_empty() {
            let mut message = String::new();
            message.push_str(&format!(
                "Failed to upload all files, {0} error(s) occurred:",
                errors.len()
            ));

            let mut unique = HashSet::new();

            for err in errors {
                unique.insert(err.to_string());
            }

            for u in unique {
                message.push_str(&style("\n=> ").dim().to_string());
                message.push_str(&u);
            }

            message
        } else {
            "Incorrect number of image/metadata pairs".to_string()
        };

        return Err(UploadError::Incomplete(message).into());
    }

    Ok(())
}

/// Upload the data to Bundlr.
async fn upload_data(
    assets: &HashMap<usize, AssetPair>,
    cache: &mut Cache,
    indices: &[usize],
    data_type: DataType,
    storage: &dyn StorageMethod,
    interrupted: Arc<AtomicBool>,
) -> Result<Vec<UploadError>> {
    let mut extension = HashSet::with_capacity(1);
    let mut paths = Vec::new();

    for index in indices {
        let item = match assets.get(index) {
            Some(asset_index) => asset_index,
            None => return Err(anyhow::anyhow!("Failed to get asset at index {}", index)),
        };
        // chooses the file path based on the data type
        let file_path = match data_type {
            DataType::Media => item.media.clone(),
            DataType::Metadata => item.metadata.clone(),
        };

        let path = Path::new(&file_path);
        let ext = path
            .extension()
            .and_then(OsStr::to_str)
            .expect("Failed to convert extension from unicode");
        extension.insert(String::from(ext));

        paths.push(file_path);
    }

    // validates that all files have the same extension
    let extension = if extension.len() == 1 {
        extension.iter().next().unwrap()
    } else {
        return Err(anyhow!("Invalid file extension: {:?}", extension));
    };

    let content_type = match data_type {
        DataType::Media => format!("image/{}", extension),
        DataType::Metadata => "application/json".to_string(),
    };

    // uploading data

    println!("\nSending data: (Ctrl+C to abort)");

    let pb = progress_bar_with_style(paths.len() as u64);
    let mut tasks = Vec::new();

    for file_path in paths {
        // path to the media/metadata file
        let path = Path::new(&file_path);

        // id of the asset (to be used to update the cache link)
        let asset_id = String::from(
            path.file_stem()
                .and_then(OsStr::to_str)
                .expect("Failed to convert path to unicode."),
        );

        let cache_item = match cache.items.0.get(&asset_id) {
            Some(item) => item,
            None => return Err(anyhow!("Failed to get config item at index {}", asset_id)),
        };

        tasks.push(AssetInfo {
            asset_id: asset_id.to_string(),
            file_path: String::from(path.to_str().expect("Failed to parse path from unicode.")),
            media_link: cache_item.media_link.clone(),
            data_type: data_type.clone(),
            content_type: content_type.clone(),
        });
    }

    let mut handles = Vec::new();

    for task in tasks.drain(0..cmp::min(tasks.len(), PARALLEL_LIMIT)) {
        handles.push(storage.upload_data(task));
    }

    let mut errors = Vec::new();

    while !interrupted.load(Ordering::SeqCst) && !handles.is_empty() {
        match select_all(handles).await {
            (Ok(res), _index, remaining) => {
                // independently if the upload was successful or not
                // we continue to try the remaining ones
                handles = remaining;

                if res.is_ok() {
                    let val = res?;
                    let link = val.clone().1;
                    // cache item to update
                    let item = cache.items.0.get_mut(&val.0).unwrap();

                    match data_type {
                        DataType::Media => item.media_link = link,
                        DataType::Metadata => item.metadata_link = link,
                    }
                    // updates the progress bar
                    pb.inc(1);
                } else {
                    // user will need to retry the upload
                    errors.push(UploadError::SendDataFailed(format!(
                        "Upload error: {:?}",
                        res.err().unwrap()
                    )));
                }
            }
            (Err(err), _index, remaining) => {
                errors.push(UploadError::SendDataFailed(format!(
                    "Upload error: {:?}",
                    err
                )));
                // ignoring all errors
                handles = remaining;
            }
        }

        if !tasks.is_empty() {
            // if we are half way through, let spawn more transactions
            if (PARALLEL_LIMIT - handles.len()) > (PARALLEL_LIMIT / 2) {
                // syncs cache (checkpoint)
                cache.sync_file()?;

                for task in tasks.drain(0..cmp::min(tasks.len(), PARALLEL_LIMIT / 2)) {
                    handles.push(storage.upload_data(task));
                }
            }
        }
    }

    if !errors.is_empty() {
        pb.abandon_with_message(format!("{}", style("Upload failed ").red().bold()));
    } else if !tasks.is_empty() {
        pb.abandon_with_message(format!("{}", style("Upload aborted ").red().bold()));
        return Err(UploadError::SendDataFailed("Not all files were uploaded.".to_string()).into());
    } else {
        pb.finish_with_message(format!("{}", style("Upload successful ").green().bold()));
    }

    // makes sure the cache file is updated
    cache.sync_file()?;

    Ok(errors)
}
