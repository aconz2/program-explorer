use serde::{Serialize, Deserialize};
use oci_spec::image as oci_image;

#[derive(Debug, Deserialize)]
pub struct PEImageId {
    pub digest: String,
    pub repository: String,
    pub registry: String,
    pub tag: String,
}

#[derive(Debug, Deserialize)]
pub struct PEImageIndexEntry {
    pub rootfs: String,
    pub config: oci_image::ImageConfiguration,
    pub manifest: oci_image::ImageManifest,
    pub id: PEImageId,
}

#[derive(Debug, Deserialize)]
pub struct PEImageIndex {
    pub images: Vec<PEImageIndexEntry>
}
