use anyhow::{Context, Result};

#[cfg(unix)]
use nix::unistd::{Gid, Group, Uid, User, getegid, geteuid, setgid, setgroups, setuid};

pub fn maybe_drop_privileges(config: Option<&str>) -> Result<()> {
    let Some(spec) = config else {
        return Ok(());
    };

    #[cfg(not(unix))]
    {
        let _ = spec;
        bail!("privdrop is supported only on Unix");
    }

    #[cfg(unix)]
    {
        let (user_str, group_str) = match spec.split_once(':') {
            Some((u, g)) => (u, Some(g)),
            None => (spec, None),
        };

        let target_user = resolve_user(user_str)?;
        let target_gid = match group_str {
            Some(group) => resolve_group(group)?,
            None => target_user.gid,
        };
        let target_uid = target_user.uid;

        setgroups(&[]).context("drop supplementary groups")?;
        if getegid() != target_gid {
            setgid(target_gid).with_context(|| format!("setgid to {}", target_gid.as_raw()))?;
        }
        if geteuid() != target_uid {
            setuid(target_uid).with_context(|| format!("setuid to {}", target_uid.as_raw()))?;
        }

        Ok(())
    }
}

#[cfg(unix)]
fn resolve_user(user: &str) -> Result<User> {
    if let Ok(uid) = user.parse::<u32>() {
        return User::from_uid(Uid::from_raw(uid))?
            .with_context(|| format!("user {user} not found"));
    }

    User::from_name(user)?.with_context(|| format!("user {user} not found"))
}

#[cfg(unix)]
fn resolve_group(group: &str) -> Result<Gid> {
    if let Ok(gid) = group.parse::<u32>() {
        let gid = Gid::from_raw(gid);
        return Group::from_gid(gid)?
            .map(|_| gid)
            .with_context(|| format!("group {group} not found"));
    }

    Group::from_name(group)?
        .with_context(|| format!("group {group} not found"))
        .map(|group| group.gid)
}
