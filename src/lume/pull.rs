use crate::lume::client::LumeClient;
use crate::TemplateConfig;
use log::{error, info, warn};
use reqwest::Client;
use serde_json::json;
use std::hash::{Hash, Hasher};
use tokio::time::{sleep, Duration};

/// Pull an image using the Lume API
pub async fn pull_image(
    config: &TemplateConfig,
    vm_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    match LumeClient::new() {
        Ok(lume) => {
            // Parse the image name to extract organization if included in the format org/image:tag
            let mut image_name = config.image.clone();
            let mut organization = config.organization.clone();

            // If image contains a slash, it likely has an organization prefix
            if image_name.contains('/') {
                let parts: Vec<&str> = image_name.split('/').collect();
                if parts.len() > 1 {
                    // If no explicit organization was provided, use the one from the image name
                    if organization.is_none() {
                        organization = Some(parts[0].to_string());
                    }

                    // Update image_name to only contain the repository part (after the slash)
                    image_name = parts[1..].join("/");

                    info!(
                        "Extracted organization '{}' from image name",
                        organization.as_ref().unwrap()
                    );
                    info!("Image name updated to '{}'", image_name);
                }
            }

            // Use the LumeClient's pull_image method
            lume.pull_image(
                &image_name,
                vm_name,
                config.registry.as_deref(),
                organization.as_deref(),
                true, // noCache is true
            )
            .await?;
            info!("Waiting for VM creation - this may take up to 30 minutes for large images...");

            // Wait for the pull to complete with exponential backoff
            let start_time = tokio::time::Instant::now();
            let max_timeout = Duration::from_secs(1800); // 30 minute max timeout

            // Initial backoff of 10 seconds, then increasing
            let mut backoff_seconds = 10;
            let mut attempts = 0;

            while start_time.elapsed() < max_timeout {
                attempts += 1;

                // Check if the VM exists after pulling
                match lume.get_vm(vm_name).await {
                    Ok(vm) => {
                        info!(
                            "✅ VM '{}' is now available after image pull. State: {}",
                            vm_name, vm.state
                        );
                        return Ok(());
                    }
                    Err(e) => {
                        // Calculate time elapsed and time remaining
                        let elapsed = start_time.elapsed();
                        let elapsed_minutes = elapsed.as_secs() / 60;
                        let elapsed_seconds = elapsed.as_secs() % 60;
                        let remaining = max_timeout.checked_sub(elapsed).unwrap_or_default();
                        let remaining_minutes = remaining.as_secs() / 60;

                        info!(
                            "Still waiting for image pull to complete (attempt {}, elapsed: {}m {}s, remaining: ~{}m)... {}",
                            attempts,
                            elapsed_minutes,
                            elapsed_seconds,
                            remaining_minutes,
                            e
                        );

                        // Sleep with exponential backoff, capped at 60 seconds
                        sleep(Duration::from_secs(backoff_seconds)).await;

                        // Increase backoff period for next attempt, but cap at 60 seconds
                        backoff_seconds = std::cmp::min(backoff_seconds * 2, 60);
                    }
                }

                // Every 5 minutes, query the list of all VMs to see progress
                if attempts % 15 == 0 {
                    // Approximately every 5 minutes with 20s backoff
                    info!("Checking overall VM list to monitor progress...");
                    match lume.list_vms().await {
                        Ok(vms) => {
                            info!("Current VMs in system: {}", vms.len());
                            for vm in vms {
                                info!("- {} ({}, {})", vm.name, vm.state, vm.os);
                            }
                        }
                        Err(e) => info!("Unable to list VMs: {}", e),
                    }
                }
            }

            error!("Timed out after 30 minutes waiting for image pull to complete");
            Err("Timed out waiting for image pull to complete".into())
        }
        Err(e) => {
            error!("Failed to initialize Lume client: {:?}", e);
            Err(e.into())
        }
    }
}

/// Check if an image has already been pulled, regardless of VM configuration
pub async fn check_image_exists(image: &str) -> Option<String> {
    match LumeClient::new() {
        Ok(lume) => {
            // Extract base image name without organization
            let base_image_name;
            let image_tag;

            // Parse the image string to extract name and tag
            if image.contains('/') {
                // Handle image with organization
                let parts: Vec<&str> = image.split('/').collect();
                if parts.len() > 1 {
                    // Get the part after the organization
                    let repo_part = parts[1];

                    // Split by colon to separate name and tag
                    let repo_parts: Vec<&str> = repo_part.split(':').collect();
                    base_image_name = repo_parts[0];
                    image_tag = if repo_parts.len() > 1 {
                        repo_parts[1]
                    } else {
                        "latest"
                    };
                } else {
                    // Unlikely case, but handle it anyway
                    let repo_parts: Vec<&str> = image.split(':').collect();
                    base_image_name = repo_parts[0];
                    image_tag = if repo_parts.len() > 1 {
                        repo_parts[1]
                    } else {
                        "latest"
                    };
                }
            } else {
                // Handle image without organization
                let parts: Vec<&str> = image.split(':').collect();
                base_image_name = parts[0];
                image_tag = if parts.len() > 1 { parts[1] } else { "latest" };
            }

            info!(
                "Looking for VMs with base image: {} (tag: {})",
                base_image_name, image_tag
            );

            // Attempt to list all VMs
            match lume.list_vms().await {
                Ok(vms) => {
                    // Look for template VMs with matching image
                    for vm in vms {
                        // For each VM, check if the name contains the base image name and tag
                        if vm.name.contains(base_image_name) && vm.name.contains(image_tag) {
                            info!("Found existing VM with the requested image: {}", vm.name);
                            return Some(vm.name);
                        }

                        // Also check template names that might contain the image name
                        if vm.name.starts_with("cirun-template-")
                            && vm.name.contains(&base_image_name.replace('-', ""))
                            && vm.name.contains(image_tag)
                        {
                            info!(
                                "Found existing template with the requested image: {}",
                                vm.name
                            );
                            return Some(vm.name);
                        }
                    }
                    None
                }
                Err(e) => {
                    error!(
                        "Failed to list VMs when searching for existing image: {:?}",
                        e
                    );
                    None
                }
            }
        }
        Err(e) => {
            error!(
                "Failed to initialize Lume client when searching for existing image: {:?}",
                e
            );
            None
        }
    }
}

/// Check if a template exists with the given name
pub async fn check_template_exists(template_name: &str) -> bool {
    match LumeClient::new() {
        Ok(lume) => match lume.get_vm(template_name).await {
            Ok(_) => {
                info!("Template '{}' already exists", template_name);
                true
            }
            Err(_) => {
                info!("Template '{}' does not exist", template_name);
                false
            }
        },
        Err(e) => {
            error!("Failed to initialize Lume client: {:?}", e);
            false
        }
    }
}

/// Find an existing template with matching configuration
pub async fn find_matching_template(config: &TemplateConfig) -> Option<String> {
    match LumeClient::new() {
        Ok(lume) => {
            // Attempt to list all VMs
            match lume.list_vms().await {
                Ok(vms) => {
                    // Look for template VMs with matching specs
                    for vm in vms {
                        // Check if this is a template VM (starts with cirun-template)
                        if vm.name.starts_with("cirun-template-") {
                            // Check if specs match what we need
                            if vm.cpu == config.cpu
                                && vm.memory / 1024 == config.memory as u64
                                && vm.disk_size.total / 1024 >= config.disk as u64
                                && vm.os == config.os
                            {
                                info!("Found existing template with matching specs: {}", vm.name);
                                return Some(vm.name);
                            }
                        }
                    }
                    None
                }
                Err(e) => {
                    error!(
                        "Failed to list VMs when searching for matching template: {:?}",
                        e
                    );
                    None
                }
            }
        }
        Err(e) => {
            error!(
                "Failed to initialize Lume client when searching for matching template: {:?}",
                e
            );
            None
        }
    }
}

/// Create a template VM from the image
pub async fn create_template(
    config: &TemplateConfig,
    template_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    match LumeClient::new() {
        Ok(lume) => {
            // First, check if we already have a VM with this image
            let existing_image = check_image_exists(&config.image).await;

            if let Some(existing_vm) = existing_image {
                info!(
                    "Found existing VM with image '{}': {}",
                    config.image, existing_vm
                );

                // If the existing VM is not the template we want to create, clone it
                if existing_vm != template_name {
                    info!(
                        "Cloning existing VM '{}' to create template '{}'",
                        existing_vm, template_name
                    );
                    match lume.clone_vm(&existing_vm, template_name).await {
                        Ok(_) => {
                            info!(
                                "Successfully cloned VM '{}' to '{}'",
                                existing_vm, template_name
                            );
                        }
                        Err(e) => {
                            error!(
                                "Failed to clone VM '{}' to '{}': {:?}",
                                existing_vm, template_name, e
                            );
                            // Fall back to pulling the image
                            info!("Falling back to pulling the image directly");
                            pull_image(config, template_name).await?;
                        }
                    }
                } else {
                    info!("The existing VM is already the template we want to create");
                }
            } else {
                // No existing VM with this image, need to pull
                info!(
                    "No existing VM found with image '{}', pulling it",
                    config.image
                );
                info!(
                    "Creating template '{}' from image '{}'",
                    template_name, config.image
                );
                info!("This process may take up to 30 minutes for large images");

                // Pull the image with the template name as the VM name
                pull_image(config, template_name).await?;
            }

            // Now configure the VM with the specified resources
            info!(
                "Configuring VM resources (CPU: {}, Memory: {}GB, Disk: {}GB)",
                config.cpu, config.memory, config.disk
            );

            let update_config = json!({
                "cpu": config.cpu,
                "memory": format!("{}GB", config.memory),
                "diskSize": format!("{}GB", config.disk)
            });

            let update_url = format!("{}/vms/{}", lume.get_base_url(), template_name);

            let client = Client::builder()
                .timeout(Duration::from_secs(600)) // 10 minute timeout for the configuration
                .build()?;

            info!(
                "Sending request to update VM configuration: {}",
                update_config
            );

            let response = client
                .patch(&update_url)
                .json(&update_config)
                .send()
                .await?;

            if !response.status().is_success() {
                let error_text = response.text().await?;
                error!("Failed to update template VM configuration: {}", error_text);
                return Err(
                    format!("Failed to update template VM configuration: {}", error_text).into(),
                );
            }

            // Verify the configuration was applied correctly
            match lume.get_vm(template_name).await {
                Ok(vm) => {
                    info!("Template '{}' created and configured with: CPU: {}, Memory: {}MB, Disk: {}GB",
                         template_name, vm.cpu, vm.memory / 1024, vm.disk_size.total / 1024);
                }
                Err(e) => {
                    warn!("Unable to verify template configuration: {}", e);
                }
            }

            info!(
                "✅ Template '{}' successfully created and ready for use",
                template_name
            );
            Ok(())
        }
        Err(e) => {
            error!("Failed to initialize Lume client: {:?}", e);
            Err(e.into())
        }
    }
}

/// Generate a template name based on the image configuration
pub fn generate_template_name(config: &TemplateConfig) -> String {
    // Parse the image name and tag
    let image_parts: Vec<&str> = config.image.split(':').collect();
    let image_name = image_parts[0];
    let image_tag = if image_parts.len() > 1 {
        image_parts[1]
    } else {
        "latest"
    };

    // Create a sanitized image name (replace slashes and other invalid characters)
    let sanitized_image = image_name.replace(['/', '.'], "-");

    // Create a configuration hash using registry, organization if present
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    config
        .registry
        .as_ref()
        .unwrap_or(&"default".to_string())
        .hash(&mut hasher);
    config
        .organization
        .as_ref()
        .unwrap_or(&"default".to_string())
        .hash(&mut hasher);
    config.os.hash(&mut hasher);
    config.cpu.hash(&mut hasher);
    config.memory.hash(&mut hasher);
    config.disk.hash(&mut hasher);
    let config_hash = hasher.finish() % 10000; // Limit to 4 digits for readability

    // Format: cirun-template-{image}-{tag}-{cpu}-{mem}-{config_hash}
    format!(
        "cirun-template-{}-{}-{}-{}-{:04}",
        sanitized_image, image_tag, config.cpu, config.memory, config_hash
    )
}
