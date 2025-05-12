use std::io::Cursor;

use bytes::Bytes;
use log::{error, trace};
use oci_spec::{
    distribution::Reference,
    image::{Arch, Digest, DigestAlgorithm, ImageConfiguration, ImageIndex, ImageManifest, Os},
    OciSpecError,
};
use reqwest::{header, Method, StatusCode};
use sha2::Sha256;
use tokio::io::{AsyncWrite, AsyncWriteExt};

// TODO
// - auth

const DOCKER_CONTENT_DIGEST_HEADER: &str = "docker-content-digest";

#[derive(Debug)]
pub enum Error {
    Reqwest(reqwest::Error),
    DigestMismatch,
    NoTagOrDigest,
    BothTagAndDigest,
    BadDockerContentDigest,
    Write,
    BadImageIndex,
    DigestAlgorithmNotHandled(DigestAlgorithm),
    StatusNotOk(StatusCode),
}

impl From<reqwest::Error> for Error {
    fn from(error: reqwest::Error) -> Error {
        Error::Reqwest(error)
    }
}

#[derive(Clone)]
pub struct Client {
    client: reqwest::Client,
}

pub struct ImageManifestResponse {
    digest: Digest,
    data: Bytes,
}

pub struct ImageIndexResponse {
    data: Bytes,
}

pub struct ImageConfigurationResponse {
    digest: Digest,
    data: Bytes,
}

impl ImageManifestResponse {
    pub fn data(&self) -> &Bytes {
        &self.data
    }
    pub fn digest(&self) -> &Digest {
        &self.digest
    }
    pub fn get(&self) -> Result<ImageManifest, OciSpecError> {
        ImageManifest::from_reader(Cursor::new(&self.data))
    }
}

impl ImageConfigurationResponse {
    pub fn data(&self) -> &Bytes {
        &self.data
    }
    pub fn digest(&self) -> &Digest {
        &self.digest
    }
    pub fn get(&self) -> Result<ImageConfiguration, OciSpecError> {
        ImageConfiguration::from_reader(Cursor::new(&self.data))
    }
}

impl ImageIndexResponse {
    pub fn data(&self) -> &Bytes {
        &self.data
    }
    pub fn get(&self) -> Result<ImageIndex, OciSpecError> {
        ImageIndex::from_reader(Cursor::new(&self.data))
    }
}

enum TagOrDigest<'a> {
    Tag(&'a str),
    Digest(&'a str),
}

impl<'a> TagOrDigest<'a> {
    fn try_from(r: &'a Reference) -> Result<Self, Error> {
        match (r.tag(), r.digest()) {
            (Some(tag), None) => Ok(TagOrDigest::Tag(tag)),
            (None, Some(digest)) => Ok(TagOrDigest::Digest(digest)),
            // from looking at the current code, this is unreachable as tag will get filled win
            // with latest
            (None, None) => Err(Error::NoTagOrDigest),
            // this is also not reachable I don't think
            (Some(_), Some(_)) => Err(Error::BothTagAndDigest),
        }
    }
    fn as_str(&'a self) -> &'a str {
        match self {
            Self::Tag(s) => s,
            Self::Digest(s) => s,
        }
    }
}

impl Client {
    pub fn new() -> Result<Self, Error> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(2))
            .build()?;
        Ok(Client { client })
    }

    pub async fn get_image_manifest(
        &mut self,
        reference: &Reference,
    ) -> Result<Option<ImageManifestResponse>, Error> {
        let content_type = "application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";
        Ok(self
            .get_manifest(reference, content_type)
            .await?
            .map(|(digest, data)| {
                // this is a weird situation with the spec, the digest isn't required to be sent,
                // but I don't think its specified what digest to use otherwise
                // ultimately I guess this is moot when looking up in the index because you get
                // the digest in there
                let digest = digest.unwrap_or_else(|| digest_from_data(&data));
                ImageManifestResponse { data, digest }
            }))
    }

    pub async fn get_image_index(
        &mut self,
        reference: &Reference,
    ) -> Result<Option<ImageIndexResponse>, Error> {
        Ok(self
            .get_manifest(reference, "application/vnd.oci.image.index.v1+json")
            .await?
            .map(|(_digest, data)| ImageIndexResponse { data }))
    }

    pub async fn lookup_image_index(
        &mut self,
        reference: &Reference,
        arch: Arch,
        os: Os,
    ) -> Result<Option<Digest>, Error> {
        if let Some(index) = self.get_image_index(reference).await? {
            let index = index.get().map_err(|_| Error::BadImageIndex)?;
            let digest = index
                .manifests()
                .iter()
                .find(|descriptor| {
                    descriptor
                        .platform()
                        .as_ref()
                        .map(|platform| *platform.architecture() == arch && *platform.os() == os)
                        .unwrap_or(false)
                })
                .map(|descriptor| descriptor.digest().clone());
            Ok(digest)
        } else {
            Ok(None)
        }
    }

    pub async fn get_image_configuration(
        &mut self,
        reference: &Reference,
        digest: &Digest,
    ) -> Result<Option<ImageConfigurationResponse>, Error> {
        let response = self.request_blob(reference, digest).await?;
        trace!(
            "domain={:?} addr={:?}",
            response.url().domain(),
            response.remote_addr()
        );

        match response.status() {
            StatusCode::OK => {
                //let expected_digest = get_docker_content_digest(&response)?;
                let data = response.bytes().await?;
                check_data_matches_digest(Some(digest), &data)?;
                Ok(Some(ImageConfigurationResponse {
                    data,
                    digest: digest.clone(),
                }))
            }
            StatusCode::NOT_FOUND => Ok(None),
            status => Err(Error::StatusNotOk(status)),
        }
    }

    pub async fn get_blob(
        &mut self,
        reference: &Reference,
        digest: &Digest,
        writer: &mut (impl AsyncWrite + std::marker::Unpin),
    ) -> Result<Option<usize>, Error> {
        let mut response = self.request_blob(reference, digest).await?;
        trace!(
            "domain={:?} addr={:?}",
            response.url().domain(),
            response.remote_addr()
        );

        match response.status() {
            StatusCode::OK => {}
            StatusCode::NOT_FOUND => {
                return Ok(None);
            }
            status => {
                return Err(Error::StatusNotOk(status));
            }
        }

        let mut len = 0;

        // how to be polymorphic over algo better?
        match digest.algorithm() {
            DigestAlgorithm::Sha256 => {
                use sha2::Digest;
                let mut hasher = Sha256::new();
                while let Some(chunk) = response.chunk().await? {
                    len += chunk.len();
                    hasher.update(&chunk);
                    writer.write_all(&chunk).await.map_err(|_| Error::Write)?;
                }
                check_digest_matches(digest, hasher)?;
            }
            algo => {
                error!("blob algo not handled {}", algo);
                return Err(Error::DigestAlgorithmNotHandled(algo.clone()));
            }
        };

        Ok(Some(len))
    }

    async fn get_manifest(
        &mut self,
        reference: &Reference,
        accept: &str,
    ) -> Result<Option<(Option<Digest>, Bytes)>, Error> {
        let domain = reference.resolve_registry();
        let repo = reference.repository();
        let td = TagOrDigest::try_from(reference)?;

        let url = format!("https://{domain}/v2/{repo}/manifests/{}", td.as_str());

        trace!("GET {url}");
        let request = self
            .client
            .request(Method::GET, &url)
            .header(header::ACCEPT, accept);

        let response = request.send().await?;
        trace!(
            "domain={:?} addr={:?}",
            response.url().domain(),
            response.remote_addr()
        );

        match response.status() {
            StatusCode::OK => {
                let digest = get_docker_content_digest(&response)?;
                let data = response.bytes().await?;
                check_data_matches_digest(digest.as_ref(), &data)?;
                Ok(Some((digest, data)))
            }
            StatusCode::NOT_FOUND => Ok(None),
            status => Err(Error::StatusNotOk(status)),
        }
    }

    async fn request_blob(
        &mut self,
        reference: &Reference,
        digest: &Digest,
    ) -> Result<reqwest::Response, Error> {
        let domain = reference.resolve_registry();
        let repo = reference.repository();
        let url = format!(
            "https://{domain}/v2/{repo}/blobs/{}:{}",
            digest.algorithm().as_ref(),
            digest.digest()
        );
        trace!("GET {url}");
        Ok(self.client.request(Method::GET, &url).send().await?)
    }
}

fn digest_from_data(x: impl AsRef<[u8]>) -> Digest {
    use sha2::Digest;
    use std::str::FromStr;
    oci_spec::image::Sha256Digest::from_str(&hex::encode(Sha256::digest(x)))
        .unwrap()
        .into()
}

fn get_docker_content_digest(response: &reqwest::Response) -> Result<Option<Digest>, Error> {
    response
        .headers()
        .get(DOCKER_CONTENT_DIGEST_HEADER)
        .map(|header_value| -> Result<Digest, Error> {
            header_value
                .to_str()
                .map_err(|_| Error::BadDockerContentDigest)?
                .try_into()
                .map_err(|_| Error::BadDockerContentDigest)
        })
        .transpose()
}

fn check_digest_matches(expected: &Digest, digest: impl sha2::Digest) -> Result<(), Error> {
    if digest_eq(expected.digest(), digest) {
        Ok(())
    } else {
        Err(Error::DigestMismatch)
    }
}

fn check_data_matches_digest(expected: Option<&Digest>, data: &[u8]) -> Result<(), Error> {
    if let Some(expected) = expected {
        if data_matches_digest(expected, data)? {
            Ok(())
        } else {
            Err(Error::DigestMismatch)
        }
    } else {
        Ok(())
    }
}

fn data_matches_digest(expected: &Digest, data: &[u8]) -> Result<bool, Error> {
    match expected.algorithm() {
        DigestAlgorithm::Sha256 => {
            use sha2::Digest;
            let mut hasher = Sha256::new();
            hasher.update(data);
            Ok(digest_eq(expected.digest(), hasher))
        }
        algo => {
            error!("manifest algo not handled {}", algo);
            Err(Error::DigestAlgorithmNotHandled(algo.clone()))
        }
    }
}

// is this too weird? it checks without allocating
// oci::image::Digest guarantees the format of the digest string for length and lower hex
// instead of decoding the digest string into bytes, we encode the digest bytes into strings one
// nibble at a time
// requires digest_lower_hex_str to be lower hex and it was produced with an algo matching the
// passed in Digest
fn digest_eq(digest_lower_hex_str: &str, digest: impl sha2::Digest) -> bool {
    let digest_bytes = digest.finalize();
    let l = digest_lower_hex_str.len();
    if l != 2 * digest_bytes.len() {
        return false;
    }

    // table mapping nibble to lower hex ascii
    #[rustfmt::skip]
    const LUT: [u8; 16] = [
        //0  1   2   3   4   5   6   7   8   9
        48, 49, 50, 51, 52, 53, 54, 55, 56, 57,
        //a  b   c    d    e    f
        97, 98, 99, 100, 101, 102,
    ];
    // checked length was even
    let as_byte_pairs = <str as AsRef<[u8]>>::as_ref(digest_lower_hex_str).chunks_exact(2);

    as_byte_pairs
        .zip(digest_bytes)
        .all(|(pair, byte): (&[u8], u8)| {
            LUT[(byte >> 4) as usize] == pair[0] && LUT[(byte & 0xf) as usize] == pair[1]
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_digest_eq() {
        fn sha256_digest(data: impl AsRef<[u8]>) -> impl sha2::Digest {
            use sha2::Digest;
            let mut hasher = Sha256::new();
            hasher.update(data);
            hasher
        }
        assert_eq!(
            true,
            digest_eq(
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
                sha256_digest("abc"),
            )
        );
        assert_eq!(
            false,
            digest_eq(
                // missing last char
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015a",
                sha256_digest("abc"),
            )
        );
        assert_eq!(
            false,
            digest_eq(
                // wrong last char
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ae",
                sha256_digest("abc"),
            )
        );
    }
}
