//! Dépendances natives (PDFium, ONNX Runtime, modèles OCR) : recherche sur le disque, vérification de
//! version et téléchargement depuis leur source officielle, d'après le manifeste `deps.toml`.
//!
//! Une version différente de celle testée n'est jamais refusée : elle est signalée, et l'utilisateur
//! peut revenir à la version testée si quelque chose ne fonctionne pas.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Manifeste intégré à l'exécutable : l'application sait quoi télécharger même installée seule.
const MANIFEST: &str = include_str!("../deps.toml");

/// Variable d'environnement qui remplace le dossier des dépendances de l'utilisateur
/// (installation portable, tests).
pub const DEPS_DIR_ENV: &str = "KITE_DEPS_DIR";

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Dependency {
    pub name: String,
    pub version: String,
    /// Sans elle, l'application ne peut pas afficher de PDF.
    pub required: bool,
    pub url: String,
    /// Fichier à extraire quand `url` désigne une archive `.zip` ou `.tgz`.
    #[serde(default)]
    pub archive_member: Option<String>,
    /// Chemin d'installation, relatif à un dossier de dépendances.
    pub destination: String,
    /// Empreinte du fichier installé dans la version testée.
    pub sha256: String,
}

#[derive(Deserialize)]
struct Manifest {
    dependency: Vec<Dependency>,
}

pub fn manifest() -> Vec<Dependency> {
    toml::from_str::<Manifest>(MANIFEST)
        .map(|m| m.dependency)
        .expect("deps.toml invalide")
}

/// Fichier installé dont l'empreinte diffère de la version testée.
#[derive(Debug, Clone, PartialEq)]
pub struct UntestedVersion {
    pub dependency: Dependency,
    pub path: PathBuf,
    pub found_sha256: String,
}

impl UntestedVersion {
    pub fn warning(&self) -> String {
        format!(
            "{} ({}) n'est pas la version testée ({}). Si quelque chose ne fonctionne pas, \
             installez cette version : {}",
            self.dependency.name,
            self.path.display(),
            self.dependency.version,
            self.dependency.url
        )
    }
}

/// Dossier où l'application installe elle-même les dépendances manquantes.
pub fn user_deps_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(DEPS_DIR_ENV) {
        return PathBuf::from(dir);
    }
    crate::settings::local_data_dir().join("deps")
}

/// Dossiers fouillés, dans l'ordre : celui de l'exécutable et ses parents (installation, `cargo run`),
/// puis celui de l'utilisateur. Jamais le dossier courant, qui dépend de la façon dont on lance
/// l'application et pourrait fournir une DLL étrangère.
pub fn search_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os(DEPS_DIR_ENV) {
        roots.push(PathBuf::from(dir));
    }
    if let Ok(exe) = std::env::current_exe() {
        roots.extend(exe.ancestors().skip(1).take(4).map(Path::to_path_buf));
    }
    roots.push(user_deps_dir());
    roots.dedup();
    roots
}

/// Emplacement d'une dépendance installée, s'il y en a une.
pub fn locate(dep: &Dependency) -> Option<PathBuf> {
    search_roots()
        .into_iter()
        .map(|root| root.join(&dep.destination))
        .find(|p| p.is_file())
}

/// Dépendance manifestée dont la destination se termine par `file_name`.
pub fn by_file_name(file_name: &str) -> Option<Dependency> {
    manifest()
        .into_iter()
        .find(|d| Path::new(&d.destination).file_name().is_some_and(|n| n == file_name))
}

pub fn missing() -> Vec<Dependency> {
    manifest().into_iter().filter(|d| locate(d).is_none()).collect()
}

/// Dépendances installées dans une version autre que celle testée. Lit et hache les fichiers :
/// à appeler hors du thread de l'interface.
pub fn untested_versions() -> Vec<UntestedVersion> {
    manifest()
        .into_iter()
        .filter_map(|dep| {
            let path = locate(&dep)?;
            let found_sha256 = sha256_file(&path).ok()?;
            (!found_sha256.eq_ignore_ascii_case(&dep.sha256)).then_some(UntestedVersion {
                dependency: dep,
                path,
                found_sha256,
            })
        })
        .collect()
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Résultat d'une installation réussie.
#[derive(Debug, Clone, PartialEq)]
pub enum Installed {
    TestedVersion,
    /// Le fichier téléchargé n'est pas celui qui a été testé (la source a changé de contenu).
    UntestedVersion(UntestedVersion),
}

/// Télécharge `dep` dans `root`, extrait le fichier utile si c'est une archive, puis le met en place
/// d'un seul coup (jamais de fichier à moitié écrit à sa place finale).
/// `progress(reçus, total)` est appelé pendant le téléchargement.
pub fn install(
    dep: &Dependency,
    root: &Path,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<Installed> {
    let destination = root.join(&dep.destination);
    if let Some(dir) = destination.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("Impossible de créer {}", dir.display()))?;
    }
    let download = destination.with_extension("download");
    let staged = destination.with_extension("part");
    let result: Result<Installed> = (|| {
        fetch(&dep.url, &download, progress)?;
        match &dep.archive_member {
            Some(member) => extract_member(&download, &dep.url, member, &staged)?,
            None => std::fs::rename(&download, &staged)?,
        }
        let found_sha256 = sha256_file(&staged)?;
        std::fs::rename(&staged, &destination).with_context(|| {
            format!("Impossible d'installer {} (fichier utilisé ?)", destination.display())
        })?;
        Ok(if found_sha256.eq_ignore_ascii_case(&dep.sha256) {
            Installed::TestedVersion
        } else {
            Installed::UntestedVersion(UntestedVersion {
                dependency: dep.clone(),
                path: destination.clone(),
                found_sha256,
            })
        })
    })();
    let _ = std::fs::remove_file(&download);
    let _ = std::fs::remove_file(&staged);
    result.with_context(|| format!("{} : échec de l'installation depuis {}", dep.name, dep.url))
}

fn fetch(url: &str, to: &Path, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<()> {
    let agent = ureq::AgentBuilder::new()
        .try_proxy_from_env(true)
        .user_agent(concat!("kite-pdf/", env!("CARGO_PKG_VERSION")))
        .build();
    let response = agent.get(url).call().context("téléchargement impossible")?;
    let total = response
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok());
    let mut reader = response.into_reader();
    let mut file = std::fs::File::create(to)?;
    let mut buffer = vec![0u8; 1 << 16];
    let mut received = 0u64;
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        file.write_all(&buffer[..n])?;
        received += n as u64;
        progress(received, total);
    }
    file.flush()?;
    if let Some(total) = total {
        if received != total {
            bail!("téléchargement interrompu ({received} octets sur {total})");
        }
    }
    Ok(())
}

fn extract_member(archive: &Path, url: &str, member: &str, to: &Path) -> Result<()> {
    let mut out = std::fs::File::create(to)?;
    if url.ends_with(".zip") {
        let mut zip = zip::ZipArchive::new(std::fs::File::open(archive)?)?;
        let mut entry = zip
            .by_name(member)
            .with_context(|| format!("{member} absent de l'archive"))?;
        std::io::copy(&mut entry, &mut out)?;
        return Ok(());
    }
    if url.ends_with(".tgz") || url.ends_with(".tar.gz") {
        let gz = flate2::read::GzDecoder::new(std::fs::File::open(archive)?);
        let mut tar = tar::Archive::new(gz);
        for entry in tar.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.to_string_lossy().replace('\\', "/");
            if path.trim_start_matches("./") == member {
                std::io::copy(&mut entry, &mut out)?;
                return Ok(());
            }
        }
        bail!("{member} absent de l'archive");
    }
    bail!("format d'archive inconnu : {url}")
}

/// `--fetch-deps` : installe les dépendances manquantes dans `root` et affiche un compte rendu.
/// Retourne le code de sortie du processus.
pub fn fetch_missing_cli(root: &Path) -> i32 {
    println!("Dépendances : {}", root.display());
    let mut failures = 0;
    for dep in manifest() {
        let target = root.join(&dep.destination);
        if target.is_file() {
            match sha256_file(&target) {
                Ok(h) if h.eq_ignore_ascii_case(&dep.sha256) => {
                    println!("  ok        {} {}", dep.name, dep.version)
                }
                Ok(_) => println!(
                    "  ATTENTION {} : ce n'est pas la version testée ({}). Si quelque chose ne \
                     fonctionne pas, supprimez {} et relancez --fetch-deps.",
                    dep.name,
                    dep.version,
                    target.display()
                ),
                Err(e) => println!("  ERREUR    {} : {e:#}", dep.name),
            }
            continue;
        }
        print!("  ...       {} {} ", dep.name, dep.version);
        let _ = std::io::stdout().flush();
        let mut last_percent = 0;
        let outcome = install(&dep, root, &mut |received, total| {
            if let Some(total) = total.filter(|t| *t > 0) {
                let percent = received * 100 / total;
                if percent >= last_percent + 10 {
                    last_percent = percent;
                    print!(".");
                    let _ = std::io::stdout().flush();
                }
            }
        });
        match outcome {
            Ok(Installed::TestedVersion) => println!(" installé"),
            Ok(Installed::UntestedVersion(u)) => {
                println!(" installé\n  ATTENTION {}", u.warning())
            }
            Err(e) => {
                failures += 1;
                println!(" ÉCHEC\n            {e:#}");
            }
        }
    }
    if failures > 0 {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manifest_is_complete() {
        let deps = manifest();
        assert!(deps.iter().any(|d| d.required && d.destination == "pdfium.dll"));
        for dep in &deps {
            assert_eq!(dep.sha256.len(), 64, "{}", dep.name);
            assert!(dep.url.starts_with("https://"), "{}", dep.name);
        }
        assert!(by_file_name("onnxruntime.dll").is_some());
    }

    #[test]
    fn test_extract_member_from_zip_and_hash() {
        let dir = std::env::temp_dir().join("kite_deps_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("a.zip");
        {
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
            zip.start_file("lib/x.dll", zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(b"contenu").unwrap();
            zip.finish().unwrap();
        }
        let out = dir.join("x.dll");
        extract_member(&archive, "https://exemple/a.zip", "lib/x.dll", &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"contenu");
        assert_eq!(sha256_file(&out).unwrap(), hex(&Sha256::digest(b"contenu")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
