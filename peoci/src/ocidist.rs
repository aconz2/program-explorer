use std::collections::BTreeMap;
use std::io::Cursor;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use log::{error, trace};
use moka::{Expiry, future::Cache};
use oci_spec::{
    OciSpecError,
    distribution::Reference,
    image::{
        Arch, Descriptor, Digest, DigestAlgorithm, ImageConfiguration, ImageIndex, ImageManifest,
        Os,
    },
};
use reqwest::{Method, StatusCode, header, header::HeaderValue};
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
    NoImageIndexForRefWithDigest,
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

// our key is index.docker.io/library/gcc for example
// does not include scope because we are always just pulling
// annoyingly ghcr.io for example doesn't care and if you get a token without scope it will work on
// everything, so we don't have to get one token per repo, but just doing it
#[derive(PartialEq, Eq, Hash)]
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
        Some(value.expires_in)
    }
}

#[derive(Debug)]
pub enum Auth {
    None,
    UserPass(String, String),
}

pub type AuthMap = BTreeMap<String, Auth>;

#[derive(Clone)]
pub struct Client {
    client: reqwest::Client,
    token_cache: Cache<TokenCacheKey, Token>,
    auth_store: Arc<RwLock<AuthMap>>,
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
            .expire_after(ExpireToken)
            .build();

        let auth_store = Arc::new(BTreeMap::new().into());

        Ok(Client {
            client,
            token_cache,
            auth_store,
        })
    }

    pub async fn set_auth(&self, auth: AuthMap) {
        *self.auth_store.write().await = auth;
    }

    pub async fn get_image_manifest(
        &mut self,
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
        &mut self,
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

    pub async fn get_matching_descriptor_from_index(
        &mut self,
        reference: &Reference,
        arch: Arch,
        os: Os,
    ) -> Result<Option<Descriptor>, Error> {
        if let Some(index) = self.get_image_index(reference).await? {
            let index = index.get()?;
            index
                .manifests()
                .iter()
                .find(|descriptor| {
                    descriptor
                        .platform()
                        .as_ref()
                        .map(|platform| *platform.architecture() == arch && *platform.os() == os)
                        .unwrap_or(false)
                })
                .map(|descriptor| Ok(descriptor.clone()))
                .transpose()
        } else {
            Ok(None)
        }
    }

    pub async fn get_image_configuration(
        &mut self,
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
            status => Err(Error::StatusNotOk(status)),
        }
    }

    // one nagging thing here is that ideally we could take Reference or Reference+Digest or
    // Reference+Descriptor
    async fn get_manifest(
        &mut self,
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
            status => Err(Error::StatusNotOk(status)),
        }
    }

    pub async fn get_blob(
        &mut self,
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
            status => {
                return Err(Error::StatusNotOk(status));
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
        &mut self,
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
        match self.auth_store.read().await.get(registry) {
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
        &mut self,
        reference: &Reference,
        mut req: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, Error> {
        // this is safe because we are only doing GET's
        // not sure if there is a better way to retry than cloning up front
        let req_copy = req.try_clone().unwrap();

        if let Some(token) = self.token_cache.get(&reference.into()).await {
            req = req.bearer_auth(token.token);
        }

        let res = req.send().await?;
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

        req_copy
            .bearer_auth(token.token)
            .send()
            .await
            .map_err(Into::into)
    }
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
}
