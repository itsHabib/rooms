from pathlib import Path
p=Path('/home/rooms/src/src/restore_exec.rs')
s=p.read_text()
s=s.replace('    let mut resume_delivery = None;', '    let mut resume_delivery = None;\n    let mut _lab_uffd = None;')
needle='                transport::api_put(\n                    launch.socket(),\n                    "/snapshot/load",'
replacement='                _lab_uffd = Some(LabUffd::start(&jail_root, fc_uid, fc_gid).await?);\n                transport::api_put(\n                    launch.socket(),\n                    "/snapshot/load",'
assert needle in s
s=s.replace(needle,replacement).replace('"backend_type": "File",','"backend_type": "Uffd",').replace('"backend_path": mem_backend_path,','"backend_path": "uffd.sock",').replace('                mem_backend_path,\n            } => {','                mem_backend_path: _,\n            } => {')
s+='\n// Disposable lab variant: no production switch or public API.\nstruct LabUffd(std::process::Child);\nimpl LabUffd {\n    async fn start(jail: &Path, uid: u32, gid: u32) -> anyhow::Result<Self> {\n        std::fs::copy("/usr/local/libexec/rooms-lab-uffd", jail.join("uffd-handler"))?;\n        let child = std::process::Command::new("/usr/sbin/chroot")\n            .arg(format!("--userspec={uid}:{gid}"))\n            .arg(format!("--groups={gid}"))\n            .arg(jail).args(["/uffd-handler", "/uffd.sock", "/snapshot.mem"])\n            .env_clear().spawn()?;\n        let mut guard = Self(child);\n        for _ in 0..200 {\n            if let Some(status) = guard.0.try_wait()? {\n                anyhow::bail!("lab UFFD handler exited before readiness: {status}");\n            }\n            if jail.join("uffd.sock").exists() { return Ok(guard); }\n            tokio::time::sleep(std::time::Duration::from_millis(10)).await;\n        }\n        anyhow::bail!("lab UFFD socket readiness timed out")\n    }\n}\nimpl Drop for LabUffd {\n    fn drop(&mut self) {\n        let _ = self.0.kill();\n        let _ = self.0.wait();\n    }\n}\n'
s=s.replace("pub struct Restored {", "pub struct Restored {\n    pub lab_uffd: Option<LabUffd>,")
s=s.replace("    let mut _lab_uffd = None;", "    let mut lab_uffd = None;")
s=s.replace("_lab_uffd = Some(", "lab_uffd = Some(")
s=s.replace("Ok((slot, witness_capture)) => Ok(Restored {", "Ok((slot, witness_capture, lab_uffd)) => Ok(Restored {\n            lab_uffd,")
s=s.replace("anyhow::Result<(room::Slot, Option<witness::Capture>)>", "anyhow::Result<(room::Slot, Option<witness::Capture>, Option<LabUffd>)>")
s=s.replace("Ok((slot, witness_capture))", "Ok((slot, witness_capture, lab_uffd))")
s=s.replace("struct LabUffd(", "pub struct LabUffd(")
s=s.replace("    validate_prepared_request_paths(config, &req, prepared)?;", "    anyhow::ensure!(!req.keep, \"lab UFFD supports command lifecycle only\");\n    validate_prepared_request_paths(config, &req, prepared)?;")
p.write_text(s)
p=Path('/home/rooms/src/src/main.rs');s=p.read_text()
s=s.replace('let rooms::restore_exec::Restored {', 'let rooms::restore_exec::Restored {\n        lab_uffd: _lab_uffd,')
p.write_text(s)
