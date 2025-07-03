use std::collections::BTreeMap;
use std::io::Cursor;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use log::{error, info, trace, warn};
use moka::{Expiry, future::Cache};
use oci_spec::{
    OciSpecError,
    distribution::Reference,
    image::{
        Arch, Descriptor, Digest, DigestAlgorithm, ImageConfiguration, ImageIndex, ImageManifest,
        Os,
    },
};
use reqwest::{Method, Response, StatusCode, header, header::HeaderValue};
use serde::Deserialize;
use sha2::Sha256;
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    sync::RwLock,
};

const DOCKER_CONTENT_DIGEST_HEADER: &str = "docker-content-digest";
const OCI_IMAGE_INDEX_V1: &str = "application/vnd.oci.image.index.v1+json";
const OCI_IMAGE_MANIFEST_V1: &str = "application/vnd.oci.image.manifest.v1+json";
const DOCKER_IMAGE_MANIFEST_V2: &str = "application/vnd.docker.distribution.manifest.v2+json";
const DOCKER_IMAGE_MANIFEST_LIST_V2: &str =
    "application/vnd.docker.distribution.manifest.list.v2+json";

const ACCEPTED_IMAGE_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";
const ACCEPTED_IMAGE_INDEX: &str = "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    Reqwest(#[from] reqwest::Error),
    OciSpecError(#[from] OciSpecError),
    DigestMismatch,
    SizeMismatch,
    NoTagOrDigest,
    BothTagAndDigest,
    BadDigest,
    BadDockerContentDigest,
    Write,
    BadImageIndex,
    InvalidAuth,
    Unknown,
    NoMatchingManifest,
    NoImageIndexForRefWithDigest,
    RatelimitExceeded,
    DomainNotSupported(String),
    BadContentType(String),
    DigestAlgorithmNotHandled(DigestAlgorithm),
    StatusNotOk(StatusCode),
    RegistryNotSupported(String),
}

// how wrong is this?
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

// NOTES
// for both ocidist::Client and ocidst_cache::Client, the whole thing is Clone, mostly modeled
// after moka::Cache being clone and using interior mutability so that everything takes &self
// idk if this is "right" and whether the type should rather be a single field of Arc with more
// things inside. Also idk if Arc<ArcSwap<_>> is a good idiom but follows from each field being
// Clone

// our key is index.docker.io/library/gcc for example
// does not include scope because we are always just pulling
// annoyingly ghcr.io for example doesn't care and if you get a token without scope it will work on
// everything, so we don't have to get one token per repo, but just doing it
#[derive(PartialEq, Eq, Hash, Debug)]
struct TokenCacheKey(String);

impl From<&Reference> for TokenCacheKey {
    fn from(reference: &Reference) -> Self {
        Self(format!(
            "{}/{}",
            reference.resolve_registry(),
            reference.repository()
        ))
    }
}

#[derive(Clone)]
struct Token {
    token: String,
    expires_in: Duration,
}

#[derive(Default)]
struct ExpireToken;

impl Expiry<TokenCacheKey, Token> for ExpireToken {
    fn expire_after_create(
        &self,
        _key: &TokenCacheKey,
        value: &Token,
        _current_time: Instant,
    ) -> Option<Duration> {
        trace!("{_key:?} expires in {:?}", value.expires_in);
        Some(value.expires_in)
    }
}

#[derive(Debug)]
pub enum Auth {
    None,
    UserPass(String, String),
}

type UtcInstant = DateTime<Utc>;

pub type AuthMap = BTreeMap<String, Auth>;
pub type RatelimitMap = BTreeMap<String, UtcInstant>;

#[derive(Clone)]
pub struct Client {
    client: reqwest::Client,
    token_cache: Cache<TokenCacheKey, Token>,
    auth_store: Arc<ArcSwap<AuthMap>>,
    ratelimit: Arc<RwLock<RatelimitMap>>,
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
            // quay.io/fedora/fedora:latest@sha256:fff will get parsed with both tag and digest
            // but when requesting from the registry, we can only supply one. I think this choice
            // makes sense but is maybe iffy
            (Some(_), Some(digest)) | (None, Some(digest)) => Ok(TagOrDigest::Digest(digest)),
            // from looking at the current code, this is unreachable as tag will get filled win
            // with latest
            (None, None) => Err(Error::NoTagOrDigest),
            // this is also not reachable I don't think
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
            .https_only(true)
            .build()?;

        let token_cache = Cache::builder()
            .max_capacity(10_000_000)
            .weigher(|k: &TokenCacheKey, v: &Token| {
                (k.0.len() + v.token.len()).try_into().unwrap_or(u32::MAX)
            })
            .eviction_listener(move |k, _v, reason| {
                trace!("token eviction {k:?} {reason:?}");
            })
            .expire_after(ExpireToken)
            .build();

        let auth_store = Arc::new(ArcSwap::from_pointee(BTreeMap::new()));
        let ratelimit = Arc::new(RwLock::new(BTreeMap::new()));

        Ok(Client {
            client,
            token_cache,
            auth_store,
            ratelimit,
        })
    }

    pub async fn set_auth(&self, auth: AuthMap) {
        //*self.auth_store.write().await = auth;
        self.auth_store.store(auth.into());
    }

    pub async fn get_image_manifest(
        &self,
        reference: &Reference,
    ) -> Result<Option<ImageManifestResponse>, Error> {
        self.get_manifest(reference, ACCEPTED_IMAGE_MANIFEST)
            .await?
            .map(|(content_type, digest, data)| {
                if content_type != OCI_IMAGE_MANIFEST_V1 && content_type != DOCKER_IMAGE_MANIFEST_V2
                {
                    Err(Error::BadContentType(content_type))
                } else {
                    // this is a weird situation with the spec, the digest isn't required to be sent,
                    // but I don't think its specified what digest to use otherwise
                    // ultimately I guess this is moot when looking up in the index because you get
                    // the digest in there
                    // in get_manifest, digest will be either from the reference or from the
                    // header, so only if both of those are absent do we compute from the data
                    let digest = digest.unwrap_or_else(|| digest_from_data(&data));
                    Ok(ImageManifestResponse { data, digest })
                }
            })
            .transpose()
    }

    pub async fn get_image_index(
        &self,
        reference: &Reference,
    ) -> Result<Option<ImageIndexResponse>, Error> {
        if reference.digest().is_some() {
            return Err(Error::NoImageIndexForRefWithDigest);
        }
        self.get_manifest(reference, ACCEPTED_IMAGE_INDEX)
            .await?
            .map(|(content_type, _digest, data)| {
                if content_type != OCI_IMAGE_INDEX_V1
                    && content_type != DOCKER_IMAGE_MANIFEST_LIST_V2
                {
                    Err(Error::BadContentType(content_type))
                } else {
                    Ok(ImageIndexResponse { data })
                }
            })
            .transpose()
    }

    // so ideally we could always get the image index, then filter for the arch+os, but docker will
    // sometimes respond with a manifest when we ask for an index
    // this is a bit annoying because even though we have the manifest, we only return the
    // descriptor since this is what the caching layer is expecting. So then there will be a second
    // request to get the manifest again. We do check the manifest config.platform for matching
    // arch+os, but it could be None. Currently, if it is None and the caller isn't asking for
    // Amd64+Linux then we assume this isn't the right manifest.
    pub async fn get_matching_descriptor_from_index(
        &self,
        reference: &Reference,
        arch: Arch,
        os: Os,
    ) -> Result<Option<Descriptor>, Error> {
        let Some((content_type, digest, data)) =
            self.get_manifest(reference, ACCEPTED_IMAGE_INDEX).await?
        else {
            return Ok(None);
        };
        if content_type == OCI_IMAGE_INDEX_V1 || content_type == DOCKER_IMAGE_MANIFEST_LIST_V2 {
            let index_response = ImageIndexResponse { data };
            let index = index_response.get()?;
            let descriptor = index.manifests().iter().find(|descriptor| {
                descriptor
                    .platform()
                    .as_ref()
                    .map(|platform| *platform.architecture() == arch && *platform.os() == os)
                    .unwrap_or(false)
            });
            if let Some(descriptor) = descriptor {
                Ok(Some(descriptor.clone()))
            } else {
                Err(Error::NoMatchingManifest)
            }
        } else if content_type == OCI_IMAGE_MANIFEST_V1 || content_type == DOCKER_IMAGE_MANIFEST_V2
        {
            let digest = digest.unwrap_or_else(|| digest_from_data(&data));
            let manifest_response = ImageManifestResponse { data, digest };
            let manifest = manifest_response.get()?;
            if let Some(platform) = manifest.config().platform() {
                if *platform.architecture() != arch || *platform.os() != os {
                    return Err(Error::NoMatchingManifest);
                }
            } else if arch != Arch::Amd64 && os != Os::Linux {
                error!(
                    "get_matching_descriptor_from_index ref={} digest={} got image manifest instead of index and no platform on config and didn't request amd64+linux, not okay",
                    reference,
                    manifest_response.digest()
                );
                return Err(Error::NoMatchingManifest);
            } else {
                warn!(
                    "get_matching_descriptor_from_index ref={} digest={} got image manifest instead of index and no platform on config, assuming amd64+linux is okay",
                    reference,
                    manifest_response.digest()
                );
            }
            let descriptor = Descriptor::new(
                content_type.as_str().into(),
                manifest_response.data().len() as u64,
                manifest_response.digest().clone(),
            );
            Ok(Some(descriptor))
        } else {
            Err(Error::BadContentType(content_type))
        }
    }

    pub async fn get_image_configuration(
        &self,
        reference: &Reference,
        descriptor: &Descriptor,
    ) -> Result<Option<ImageConfigurationResponse>, Error> {
        let response = self.request_blob(reference, descriptor).await?;
        trace!(
            "domain={:?} addr={:?}",
            response.url().domain(),
            response.remote_addr()
        );

        match response.status() {
            StatusCode::OK => {
                let data = response.bytes().await?;
                check_data_matches_descriptor(descriptor, &data)?;
                Ok(Some(ImageConfigurationResponse {
                    data,
                    digest: descriptor.digest().clone(),
                }))
            }
            StatusCode::NOT_FOUND => Ok(None),
            _ => Err(status_not_ok(response).await),
        }
    }

    // one nagging thing here is that ideally we could take Reference or Reference+Digest or
    // Reference+Descriptor
    async fn get_manifest(
        &self,
        reference: &Reference,
        accept: &str,
    ) -> Result<Option<(String, Option<Digest>, Bytes)>, Error> {
        let domain = reference.resolve_registry();
        let repo = reference.repository();
        let td = TagOrDigest::try_from(reference)?;

        let url = format!("https://{domain}/v2/{repo}/manifests/{}", td.as_str());

        trace!("GET {url}");
        let request = self
            .client
            .request(Method::GET, &url)
            .header(header::ACCEPT, accept);

        let response = self.auth_and_retry(reference, request).await?;
        trace!(
            "domain={:?} addr={:?}",
            response.url().domain(),
            response.remote_addr()
        );

        match response.status() {
            StatusCode::OK => {
                let digest = if let TagOrDigest::Digest(s) = td {
                    Some(s.parse().map_err(|_| Error::BadDigest)?)
                } else {
                    get_docker_content_digest(&response)?
                };
                let content_type = response
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .map(|x| x.to_str().unwrap_or("").to_string())
                    .unwrap_or_else(String::new);
                // better to hash incrementally?
                let data = response.bytes().await?;
                check_data_matches_digest(digest.as_ref(), &data)?;
                Ok(Some((content_type, digest, data)))
            }
            StatusCode::NOT_FOUND => Ok(None),
            _ => Err(status_not_ok(response).await),
        }
    }

    pub async fn get_blob(
        &self,
        reference: &Reference,
        descriptor: &Descriptor,
        writer: &mut (impl AsyncWrite + std::marker::Unpin),
    ) -> Result<Option<usize>, Error> {
        let mut response = self.request_blob(reference, descriptor).await?;
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
            _ => {
                return Err(status_not_ok(response).await);
            }
        }

        let mut len = 0;

        // how to be polymorphic over algo better?
        match descriptor.digest().algorithm() {
            DigestAlgorithm::Sha256 => {
                use sha2::Digest;
                let mut hasher = Sha256::new();
                while let Some(chunk) = response.chunk().await? {
                    len += chunk.len();
                    hasher.update(&chunk);
                    writer.write_all(&chunk).await.map_err(|_| Error::Write)?;
                }
                writer.flush().await.map_err(|_| Error::Write)?;
                if descriptor.size() != len as u64 {
                    return Err(Error::SizeMismatch);
                }
                check_digest_matches(descriptor.digest(), hasher)?;
            }
            algo => {
                error!("blob algo not handled {}", algo);
                return Err(Error::DigestAlgorithmNotHandled(algo.clone()));
            }
        };

        Ok(Some(len))
    }

    async fn request_blob(
        &self,
        reference: &Reference,
        descriptor: &Descriptor,
    ) -> Result<reqwest::Response, Error> {
        let domain = reference.resolve_registry();
        let repo = reference.repository();
        let url = format!(
            "https://{domain}/v2/{repo}/blobs/{}:{}",
            descriptor.digest().algorithm().as_ref(),
            descriptor.digest().digest()
        );
        trace!("GET {url}");
        self.auth_and_retry(reference, self.client.request(Method::GET, &url))
            .await
    }

    async fn get_token_for(
        &self,
        reference: &Reference,
        www_auth: &WWWAuthenticateBearerRealmService<'_>,
    ) -> Result<Option<Token>, Error> {
        let registry = reference.resolve_registry();
        //match self.auth_store.read().await.get(registry) {
        match self.auth_store.load().get(registry) {
            Some(Auth::None) => Ok(None),
            Some(Auth::UserPass(user, pass)) => {
                let entry = self
                    .token_cache
                    .entry(reference.into())
                    .or_try_insert_with(retreive_token_user_pass(
                        self.client.clone(),
                        reference,
                        www_auth,
                        user,
                        pass,
                    ))
                    .await
                    .map_err(|e| {
                        // drop the error to go from Arc<Error> to Error
                        // TODO do something better
                        error!("error in retreive_token_user_pass {:?}", e);
                        Error::Unknown
                    })?;
                if entry.is_fresh() {
                    trace!("got new token for {}", entry.key().0);
                }
                Ok(Some(entry.into_value()))
            }
            None => Err(Error::RegistryNotSupported(registry.to_string())),
        }
    }

    // when sending a request, we first check the token cache if we have a token for the
    // registry+repo and add it if so. We then send the request and (even with an added token that
    // could have expired) there is a possibility we get 401. If so, we have to look at the
    // WWW_AUTHENTICATE header for the realm (url) and service so that we can try requesting (or
    // updating) a token. Once we get that token, we can retry the request
    // this isn't really ideal b/c we could get concurrent requests that use a stale token, but
    // then they will retry and correctly have one get the new token (assuming it has expired in
    // the cache correctly); but we have to fail the request to properly get the www-auth so idk
    // what else to do. Maybe we cache the www-auth per domain? idk
    async fn auth_and_retry(
        &self,
        reference: &Reference,
        mut req: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, Error> {
        self.check_ratelimit(reference).await?;

        // this is safe because we are only doing GET's
        // not sure if there is a better way to retry than cloning up front
        let req_copy = req.try_clone().unwrap();

        if let Some(token) = self.token_cache.get(&reference.into()).await {
            req = req.bearer_auth(token.token);
        }

        let res = req.send().await?;

        self.handle_ratelimit(reference, &res).await?;

        if log::log_enabled!(log::Level::Trace) {
            for (header, value) in res.headers().iter() {
                trace!("header {}: {:?}", header, value);
            }
        }

        if res.status() != StatusCode::UNAUTHORIZED {
            return Ok(res);
        }

        let www_auth = res
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .and_then(parse_www_authenticate_bearer_header)
            .ok_or_else(|| {
                error!(
                    "bad auth but couldn't get www-authenticate header {:?}",
                    res.headers().get(header::WWW_AUTHENTICATE)
                );
                Error::StatusNotOk(StatusCode::UNAUTHORIZED)
            })?;

        let token = self
            .get_token_for(reference, &www_auth)
            .await?
            .ok_or(Error::StatusNotOk(StatusCode::UNAUTHORIZED))?;

        let res = req_copy.bearer_auth(token.token).send().await?;

        self.handle_ratelimit(reference, &res).await?;

        Ok(res)
    }

    async fn check_ratelimit(&self, reference: &Reference) -> Result<(), Error> {
        let mut remove = false;
        let registry = reference.resolve_registry();
        if let Some(ratelimit_end) = self.ratelimit.read().await.get(registry) {
            if Utc::now() < *ratelimit_end {
                warn!("still in ratelimit reset period");
                return Err(Error::RatelimitExceeded);
            } else {
                remove = true;
            }
        }
        if remove {
            self.ratelimit.write().await.remove(registry);
        }
        Ok(())
    }

    async fn handle_ratelimit(&self, reference: &Reference, res: &Response) -> Result<(), Error> {
        // ghcr apparently returns either 403 or 429
        if !matches!(
            res.status(),
            StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
        ) {
            return Ok(());
        }

        if let Some(ratelimit_remaining) = get_ratelimit_remaining_header(res.headers()) {
            info!("parsed ratelimit header {:?}", ratelimit_remaining);
        }
        for (header, value) in res.headers().iter() {
            if header.as_str().contains("ratelimit") {
                info!("ratelimit header {}: {:?}", header, value);
            }
        }

        let registry = reference.resolve_registry();
        let end: UtcInstant = if let Some(reset) = get_ratelimit_reset_header(res.headers()) {
            let now = chrono::Utc::now();
            let time = reset
                .try_into() // u64 -> i64
                .ok()
                .and_then(|x| chrono::DateTime::<chrono::Utc>::from_timestamp(x, 0))
                .unwrap_or_else(|| {
                    error!("bad reset timestamp");
                    now + Duration::from_secs(DEFAULT_RATELIMIT_RESET)
                });
            if now > time {
                warn!("got ratelimit reset in past, assuming it is duration");
                now + Duration::from_secs(reset)
            } else {
                time
            }
        } else {
            warn!(
                "got res status {} from {} but no ratelimit-reset",
                res.status(),
                registry
            );
            chrono::Utc::now() + Duration::from_secs(DEFAULT_RATELIMIT_RESET)
        };

        warn!(
            "hit ratelimit when registry={} res.url={}",
            registry,
            res.url(),
        );
        self.ratelimit
            .write()
            .await
            .insert(registry.to_string(), end);

        Err(Error::RatelimitExceeded)
    }
}

async fn status_not_ok(res: Response) -> Error {
    let status = res.status();
    if log::log_enabled!(log::Level::Trace) {
        match res.text().await {
            Ok(body) => {
                trace!("status={}, body={}", status, body);
            }
            Err(e) => {
                trace!("unhandled error getting body, status={status}, error={e:?}");
            }
        }
    }
    Error::StatusNotOk(status)
}

async fn retreive_token_user_pass(
    client: reqwest::Client,
    reference: &Reference,
    www_auth: &WWWAuthenticateBearerRealmService<'_>,
    user: &str,
    pass: &str,
) -> Result<Token, Error> {
    #[derive(Deserialize)]
    struct JsonToken {
        token: String,
        expires_in: Option<u64>,
        //issued_at: Option<String>, // "2025-05-12T21:35:54.377188944Z" but not really useful
    }

    let scope = format!("repository:{}:pull", reference.repository());

    let token = client
        .request(Method::GET, www_auth.realm)
        .query(&[("scope", scope), ("service", www_auth.service.to_string())])
        .basic_auth(user, Some(pass))
        .send()
        .await?
        .json::<JsonToken>()
        .await?;

    // https://distribution.github.io/distribution/spec/auth/token/#token-response-fields
    // gives the default as 60 seconds
    let expires_in = Duration::from_secs(token.expires_in.unwrap_or(60));
    let token = token.token;
    Ok(Token { token, expires_in })
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

fn check_data_matches_descriptor(expected: &Descriptor, data: &[u8]) -> Result<(), Error> {
    if expected.size() != data.len() as u64 {
        Err(Error::SizeMismatch)
    } else if !data_matches_digest(expected.digest(), data)? {
        Err(Error::DigestMismatch)
    } else {
        Ok(())
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

    as_byte_pairs.zip(digest_bytes).all(|(pair, byte)| {
        LUT[(byte >> 4) as usize] == pair[0] && LUT[(byte & 0xf) as usize] == pair[1]
    })
}

#[derive(Default)]
struct WWWAuthenticateBearer<'a> {
    realm: Option<&'a str>,
    service: Option<&'a str>,
    scope: Option<&'a str>,
}

struct WWWAuthenticateBearerRealmService<'a> {
    realm: &'a str,
    service: &'a str,
}

fn parse_www_authenticate_bearer_header(
    input: &HeaderValue,
) -> Option<WWWAuthenticateBearerRealmService<'_>> {
    let res = parse_www_authenticate_bearer_str(input.to_str().ok()?)?;
    Some(WWWAuthenticateBearerRealmService {
        realm: res.realm?,
        service: res.service?,
    })
}

fn parse_www_authenticate_bearer_str(input: &str) -> Option<WWWAuthenticateBearer<'_>> {
    use nom::{
        IResult, Parser,
        bytes::{complete::tag, take_until1},
        character::complete::{alpha1, char},
        multi::{many0, many1, separated_list0},
        sequence::{delimited, preceded, separated_pair, terminated},
    };
    fn parser(input: &str) -> IResult<&str, Vec<(&str, &str)>> {
        let (input, matches) = preceded(
            terminated(tag("Bearer"), many1(tag(" "))),
            separated_list0(
                terminated(tag(","), many0(tag(" "))),
                separated_pair(
                    alpha1,
                    tag("="),
                    delimited(char('"'), take_until1("\""), char('"')),
                ),
            ),
        )
        .parse(input)?;
        Ok((input, matches))
    }
    let (_, matches) = parser(input).ok()?;
    let mut ret = WWWAuthenticateBearer::default();
    for (k, v) in matches.into_iter() {
        match k {
            "realm" => ret.realm = Some(v),
            "service" => ret.service = Some(v),
            "scope" => ret.scope = Some(v),
            _ => {}
        }
    }
    Some(ret)
}

// treating ratelimit as one word b/c that is what the http header does
//
// github doesn't (I don't think) use a window in the header value and the docs suggest the default
// is one hour
#[allow(dead_code)]
const DEFAULT_RATELIMIT_WINDOW: u32 = 60 * 60; // 1 hour in seconds
// if they don't send ratelimit-reset, default to 1 minute (guessing)
const DEFAULT_RATELIMIT_RESET: u64 = 60;

#[derive(Debug, PartialEq, Eq)]
struct RatelimitRemaining {
    quota: u32, // units
    window: Option<u32>,
}

impl RatelimitRemaining {
    #[allow(dead_code)]
    fn window_duration(&self) -> Duration {
        Duration::from_secs(self.window.unwrap_or(DEFAULT_RATELIMIT_WINDOW) as u64)
    }
}

fn get_ratelimit_remaining_header(map: &reqwest::header::HeaderMap) -> Option<RatelimitRemaining> {
    if let Some(value) = map.get("ratelimit-remaining") {
        parse_ratelimit_remaining_header(value)
    } else if let Some(value) = map.get("x-ratelimit-remaining") {
        parse_ratelimit_remaining_header(value)
    } else {
        None
    }
}

fn parse_ratelimit_remaining_header(input: &HeaderValue) -> Option<RatelimitRemaining> {
    parse_ratelimit_remaining_str(input.to_str().ok()?)
}

fn parse_ratelimit_remaining_str(input: &str) -> Option<RatelimitRemaining> {
    // nom not so nice here
    //use nom::{
    //    IResult, Parser,
    //    bytes::{complete::tag, take_while},
    //    character::complete::{digit1},
    //    sequence::{preceded},
    //    combinator::{map_res,opt},
    //};
    //fn parser(input: &str) -> IResult<&str, (u32, Option<u32>)> {
    //    let (input, quota) : (_, u32) = map_res(digit1, str::parse).parse(input)?;
    //    let (input, window) = opt(preceded(tag(";w="), map_res(digit1, str::parse))).parse(input)?;
    //    Ok((input, (quota, window)))
    //}
    //let (_, (quota, window)) = parser(input).ok()?;
    //Some(RatelimitRemaining {
    //    quota, window
    //})
    if let Some((l, r)) = input.split_once(";w=") {
        let quota = l.parse().ok()?;
        let window = Some(r.parse().ok()?);
        Some(RatelimitRemaining { quota, window })
    } else {
        let quota = input.parse().ok()?;
        Some(RatelimitRemaining {
            quota,
            window: None,
        })
    }
}

// https://www.ietf.org/archive/id/draft-polli-ratelimit-headers-02.html#section-3.3
// returns whatever number is in the header. RFC ways it is the number of seconds until reset, but
// github and docker both specify that it is the timestamp when it resets! Dockers says "unix time"
// and github says "UTC epoch seconds"
// https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api?apiVersion=2022-11-28#checking-the-status-of-your-rate-limit
// https://docs.docker.com/reference/api/hub/latest/#tag/rate-limiting
fn get_ratelimit_reset_header(map: &reqwest::header::HeaderMap) -> Option<u64> {
    if let Some(value) = map.get("ratelimit-reset") {
        parse_ratelimit_reset_header(value)
    } else if let Some(value) = map.get("x-ratelimit-reset") {
        parse_ratelimit_reset_header(value)
    } else {
        None
    }
}

fn parse_ratelimit_reset_header(input: &HeaderValue) -> Option<u64> {
    parse_ratelimit_reset_str(input.to_str().ok()?)
}

fn parse_ratelimit_reset_str(input: &str) -> Option<u64> {
    input.parse().ok()
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

    #[test]
    fn test_www_authenticate() {
        // example from https://distribution.github.io/distribution/spec/auth/token/#how-to-authenticate
        let cases = [
            r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:samalba/my-app:pull,push""#,
            r#"Bearer realm="https://auth.docker.io/token", service="registry.docker.io",scope="repository:samalba/my-app:pull,push""#,
            r#"Bearer realm="https://auth.docker.io/token", service="registry.docker.io", scope="repository:samalba/my-app:pull,push""#,
            r#"Bearer    realm="https://auth.docker.io/token",   service="registry.docker.io", scope="repository:samalba/my-app:pull,push""#,
            r#"Bearer   service="registry.docker.io", scope="repository:samalba/my-app:pull,push",realm="https://auth.docker.io/token""#,
        ];
        for case in cases.iter() {
            let x = parse_www_authenticate_bearer_str(case).unwrap();
            assert_eq!(x.realm, Some("https://auth.docker.io/token"), "{}", case);
            assert_eq!(x.service, Some("registry.docker.io"), "{}", case);
            assert_eq!(
                x.scope,
                Some("repository:samalba/my-app:pull,push"),
                "{}",
                case
            );
        }
    }

    #[test]
    fn test_ratelimit_remaining() {
        assert_eq!(
            RatelimitRemaining {
                quota: 100,
                window: None
            },
            parse_ratelimit_remaining_str("100").unwrap()
        );
        assert_eq!(
            RatelimitRemaining {
                quota: 100,
                window: Some(3600)
            },
            parse_ratelimit_remaining_str("100;w=3600").unwrap()
        );
        assert_eq!(None, parse_ratelimit_remaining_str("x100;w=3600"));
        assert_eq!(None, parse_ratelimit_remaining_str("100x;w=3600"));
    }
}
