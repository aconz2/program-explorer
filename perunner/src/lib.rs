pub mod cloudhypervisor;
pub mod iofile;
pub mod worker;

use oci_spec::runtime as oci_runtime;

use once_cell::sync::Lazy;

pub const UID: u32 = 1000;
pub const NIDS: u32 = 65534; // size of uid_gid_map

const SECCOMP_JSON: &[u8] = include_bytes!("../seccomp.json");

// TODO should we just desrialize on each access?
// kinda wish crun could take the policy directly (or precompiled) so we didn't have to shovel it
// so many times
static SECCOMP: Lazy<oci_runtime::LinuxSeccomp> =
    Lazy::new(|| serde_json::from_slice(SECCOMP_JSON).unwrap());

#[derive(Debug, thiserror::Error)]
pub enum Error {
    BadArch,
    BadOs,
    BadArgs,
    EmptyUser,
    UnhandledUser,
    OciUser,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

// NOTE: if oci_spec::image::ImageConfiguration was parsed from a vnd.docker.distribution.manifest.v2.json, I'm
// getting empty strings for a lot of things that are Option
// the allocations in this make me a bit unhappy, but maybe its okay
pub fn create_runtime_spec(
    image_config: &peoci::spec::ImageConfiguration,
    entrypoint: Option<&[String]>,
    cmd: Option<&[String]>,
    env: Option<&[String]>,
) -> Result<oci_runtime::Spec, Error> {
    // TODO multi arch/os
    if image_config.architecture != peoci::spec::Arch::Amd64 {
        return Err(Error::BadArch);
    }
    if image_config.os != peoci::spec::Os::Linux {
        return Err(Error::BadOs);
    }

    let mut spec = oci_runtime::Spec::rootless(UID, UID);
    spec.set_hostname(Some("programexplorer".to_string()));

    // doing spec.set_uid_mappings sets the volume mount idmap, not the user namespace idmap
    let map = oci_runtime::LinuxIdMappingBuilder::default()
        .host_id(UID)
        .container_id(0u32)
        .size(NIDS)
        .build()
        .unwrap();
    let linux = spec.linux_mut().as_mut().unwrap();
    linux
        .set_uid_mappings(Some(vec![map]))
        .set_gid_mappings(Some(vec![map]));

    linux.namespaces_mut().as_mut().unwrap().push(
        oci_runtime::LinuxNamespaceBuilder::default()
            .typ(oci_runtime::LinuxNamespaceType::Network)
            .build()
            .unwrap(),
    );

    linux.set_seccomp(Some(SECCOMP.clone()));

    // TODO how does oci-spec-rs deserialize the config .Env into .env ?

    {
        // we "know" that a defaulted runtime spec has Some mounts
        let mounts = spec.mounts_mut().as_mut().unwrap();

        // /tmp
        mounts.push(
            oci_runtime::MountBuilder::default()
                .destination("/tmp")
                .typ("tmpfs")
                .options(vec!["size=50%".into(), "mode=777".into()])
                .build()
                .unwrap(),
        );

        // /run/pe/input
        mounts.push(
            oci_runtime::MountBuilder::default()
                .destination("/run/pe/input")
                .typ("bind")
                .source("/run/input")
                // idk should this be readonly?
                // TODO I don't fully understand why this is rbind
                // https://docs.kernel.org/filesystems/sharedsubtree.html
                .options(vec!["rw".into(), "rbind".into()])
                .build()
                .unwrap(),
        );

        // /run/pe/output
        mounts.push(
            oci_runtime::MountBuilder::default()
                .destination("/run/pe/output")
                .typ("bind")
                .source("/run/output/dir")
                .options(vec!["rw".into(), "rbind".into()])
                .build()
                .unwrap(),
        );
    }

    // we "know" that a defaulted runtime spec has Some process
    let process = spec.process_mut().as_mut().unwrap();

    // ugh having image_config.config() return Option and config.entrypoint() return &Option messes
    // the chaining...
    let args = {
        let mut acc = vec![];
        match &image_config.config {
            Some(config) => {
                match (entrypoint, &config.entrypoint) {
                    (Some(xs), _) => {
                        acc.extend_from_slice(xs);
                    }
                    (None, Some(xs)) => {
                        acc.extend_from_slice(xs);
                    }
                    _ => {}
                }
                match (cmd, &config.cmd) {
                    (Some(xs), _) => {
                        acc.extend_from_slice(xs);
                    }
                    (None, Some(xs)) => {
                        acc.extend_from_slice(xs);
                    }
                    _ => {}
                }
            }
            None => {
                if let Some(xs) = entrypoint {
                    acc.extend_from_slice(xs);
                }
                if let Some(xs) = cmd {
                    acc.extend_from_slice(xs);
                }
            }
        }
        acc
    };
    if args.is_empty() {
        return Err(Error::BadArgs);
    }
    process.set_args(Some(args));

    // always adding PATH begrudginly https://github.com/docker-library/busybox/issues/214
    let env = {
        let mut tmp = Vec::with_capacity(8);
        // crun goes sequentially and uses putenv, so having PATH last will
        tmp.push("PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string());
        if let Some(env) = env {
            tmp.extend_from_slice(env);
        } else if let Some(config) = &image_config.config {
            if let Some(env) = &config.env {
                tmp.extend_from_slice(env);
            }
        }
        tmp
    };
    *process.env_mut() = Some(env);

    // image config can be null / totally empty
    if let Some(config) = &image_config.config {
        // TODO: handle user fully
        // from oci-spec-rs/src/image/config.rs
        // user:
        //   For Linux based systems, all
        //   of the following are valid: user, uid, user:group,
        //   uid:gid, uid:group, user:gid. If group/gid is not
        //   specified, the default group and supplementary
        //   groups of the given user/uid in /etc/passwd from
        //   the container are applied.
        // let _ = config.exposed_ports; // ignoring network for now
        if let Some(user_config_string) = &config.user {
            if !user_config_string.is_empty() {
                process.set_user(parse_user_string(user_config_string)?);
            }
        }

        if let Some(cwd) = &config.working_dir {
            if !cwd.is_empty() {
                process.set_cwd(cwd.into());
            }
        }
    } else {
        // TODO are the defaults all okay here?
    }

    Ok(spec)
}

fn parse_user_string(s: &str) -> Result<oci_runtime::User, Error> {
    if s.is_empty() {
        return Err(Error::EmptyUser);
    }
    if let Ok(uid) = s.parse::<u32>() {
        // TODO this is also supposed to lookup the gid in /etc/group I think
        return oci_runtime::UserBuilder::default()
            .uid(uid)
            .gid(uid)
            .build()
            .map_err(|_| Error::OciUser);
    }
    let mut iter = s.splitn(2, ":");
    let a = iter.next().map(|x| x.parse::<u32>());
    let b = iter.next().map(|x| x.parse::<u32>());
    match (a, b) {
        (Some(Ok(uid)), Some(Ok(gid))) => oci_runtime::UserBuilder::default()
            .uid(uid)
            .gid(gid)
            .build()
            .map_err(|_| Error::OciUser),
        _ => Err(Error::UnhandledUser),
    }
}
