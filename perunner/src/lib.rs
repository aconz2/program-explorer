pub mod cloudhypervisor;
pub mod worker;

use oci_spec::runtime as oci_runtime;
use oci_spec::image as oci_image;

pub const UID: u32 = 1000;
pub const NIDS: u32 = 1000; // size of uid_gid_map

// the allocations in this make me a bit unhappy, but maybe its all worth it
pub fn create_runtime_spec(image_config: &oci_image::ImageConfiguration, run_args: &[String]) -> Option<oci_runtime::Spec> {
    //let spec: oci_runtime::Spec = Default::default();
    let mut spec = oci_runtime::Spec::rootless(1000, 1000);
    // ugh this api is horrible
    spec.set_hostname(Some("programexplorer".to_string()));


    // doing spec.set_uid_mappings sets the volume mount idmap, not the user namespace idmap
    if true {
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
    }

    // sanity checks
    if *image_config.architecture() != oci_image::Arch::Amd64 { return None; }
    if *image_config.os() != oci_image::Os::Linux { return None; }

    // TODO how does oci-spec-rs deserialize the config .Env into .env ?

    {
        // we "know" that a defaulted runtime spec has Some mounts
        let mounts = spec.mounts_mut().as_mut().unwrap();

        // /tmp
        mounts.push(oci_runtime::MountBuilder::default()
            .destination("/tmp")
            .typ("tmpfs")
            .options(vec!["size=50%".into(), "mode=777".into()])
            .build()
            .unwrap()
            );

        // /run/pe/input
        mounts.push(oci_runtime::MountBuilder::default()
            .destination("/run/pe/input")
            .typ("bind")
            .source("/run/input")
            // idk should this be readonly?
            // TODO I don't fully understand why this is rbind
            // https://docs.kernel.org/filesystems/sharedsubtree.html
            .options(vec!["rw".into(), "rbind".into()])
            .build()
            .unwrap()
            );

        // /run/pe/output
        mounts.push(oci_runtime::MountBuilder::default()
            .destination("/run/pe/output")
            .typ("bind")
            .source("/run/output/dir")
            .options(vec!["rw".into(), "rbind".into()])
            .build()
            .unwrap()
            );
    }

    if let Some(config) = image_config.config() {
        // TODO: handle user
        // from oci-spec-rs/src/image/config.rs
        // user:
        //   For Linux based systems, all
        //   of the following are valid: user, uid, user:group,
        //   uid:gid, uid:group, user:gid. If group/gid is not
        //   specified, the default group and supplementary
        //   groups of the given user/uid in /etc/passwd from
        //   the container are applied.
        // let _ = config.exposed_ports; // ignoring network for now

        // we "know" that a defaulted runtime spec has Some process
        let process = spec.process_mut().as_mut().unwrap();

        if let Some(env) = config.env() {
            *process.env_mut() = Some(env.clone());
        }

        if run_args.is_empty() {
            let args = {
                let mut acc = vec![];
                if let Some(entrypoint) = config.entrypoint() { acc.extend_from_slice(entrypoint); }
                if let Some(cmd) = config.cmd()               { acc.extend_from_slice(cmd); }
                if acc.is_empty() { return None; }
                acc
            };
            process.set_args(Some(args));
        } else {
            process.set_args(Some(run_args.into()));
        }

        if let Some(cwd) = config.working_dir() { process.set_cwd(cwd.into()); }
    }

    Some(spec)
}

