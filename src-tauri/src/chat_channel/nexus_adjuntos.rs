//! NEXUS: photos and files sent over Telegram reach the agent.
//!
//! Upstream only understands text and voice notes (and FORGE's photos), so
//! anything else was dropped. Here the file is saved under
//! `~/.local/share/nexus/telegram/` and the agent gets a line saying where it
//! is, plus whatever caption came with it, so it can open it with its tools.

use std::path::{Path, PathBuf};

/// What came attached to a Telegram message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adjunto {
    pub file_id: String,
    pub nombre: String,
    pub tipo: &'static str,
    pub caption: Option<String>,
}

fn texto(v: Option<&serde_json::Value>) -> Option<String> {
    v.and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The largest photo, or the document / audio / video in `message`.
pub fn extraer(message: &serde_json::Value) -> Option<Adjunto> {
    let caption = texto(message.get("caption"));
    let id = message
        .get("message_id")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if let Some(foto) = crate::chat_channel::forge_tap::extract_photo(message) {
        return Some(Adjunto {
            file_id: foto.file_id,
            nombre: format!("foto-{id}.jpg"),
            tipo: "una foto",
            caption,
        });
    }
    for (campo, tipo, ext) in [
        ("document", "un archivo", "bin"),
        ("audio", "un audio", "mp3"),
        ("video", "un video", "mp4"),
        ("video_note", "un video", "mp4"),
    ] {
        let Some(obj) = message.get(campo) else {
            continue;
        };
        let Some(file_id) = texto(obj.get("file_id")) else {
            continue;
        };
        let nombre = texto(obj.get("file_name"))
            .map(|n| limpiar(&n))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("{campo}-{id}.{ext}"));
        return Some(Adjunto {
            file_id,
            nombre,
            tipo,
            caption,
        });
    }
    None
}

/// A file name that can't climb out of the folder or hide.
pub fn limpiar(nombre: &str) -> String {
    let base = nombre.rsplit(['/', '\\']).next().unwrap_or("");
    let limpio: String = base
        .chars()
        .map(|c| if c.is_control() { '_' } else { c })
        .collect();
    limpio.trim_start_matches('.').trim().to_string()
}

pub fn carpeta() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".local/share/nexus/telegram")
}

/// Where to save `nombre` inside `dir` without overwriting an earlier file.
pub fn destino(dir: &Path, fecha: &str, nombre: &str) -> PathBuf {
    let mut ruta = dir.join(format!("{fecha}-{nombre}"));
    let mut n = 2;
    while ruta.exists() {
        ruta = dir.join(format!("{fecha}-{n}-{nombre}"));
        n += 1;
    }
    ruta
}

/// The text the agent receives in place of the attachment.
pub fn mensaje(adj: &Adjunto, ruta: &Path) -> String {
    let aviso = format!(
        "[Julián mandó {} por Telegram. Está guardado en {} — ábrelo para verlo.]",
        adj.tipo,
        ruta.display()
    );
    match &adj.caption {
        Some(c) => format!("{c}\n\n{aviso}"),
        None => aviso,
    }
}

/// Saves the bytes and returns the text for the agent.
pub fn guardar(adj: &Adjunto, bytes: &[u8]) -> std::io::Result<String> {
    let dir = carpeta();
    std::fs::create_dir_all(&dir)?;
    let fecha = chrono::Local::now().format("%Y-%m-%d_%H%M%S").to_string();
    let ruta = destino(&dir, &fecha, &adj.nombre);
    std::fs::write(&ruta, bytes)?;
    Ok(mensaje(adj, &ruta))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn foto_con_texto() {
        let m = json!({"message_id": 9, "caption": " mira esto ",
            "photo": [{"file_id": "a", "file_size": 10}, {"file_id": "b", "file_size": 99}]});
        let a = extraer(&m).unwrap();
        assert_eq!((a.file_id.as_str(), a.nombre.as_str()), ("b", "foto-9.jpg"));
        assert_eq!(a.caption.as_deref(), Some("mira esto"));
    }

    #[test]
    fn archivo_con_nombre_peligroso() {
        let m = json!({"message_id": 3,
            "document": {"file_id": "d", "file_name": "../../.bashrc"}});
        let a = extraer(&m).unwrap();
        assert_eq!(a.nombre, "bashrc");
        assert_eq!(a.tipo, "un archivo");
    }

    #[test]
    fn sin_adjunto() {
        assert!(extraer(&json!({"text": "hola"})).is_none());
    }

    #[test]
    fn no_pisa_archivos() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = destino(dir.path(), "f", "x.pdf");
        std::fs::write(&p1, b"1").unwrap();
        let p2 = destino(dir.path(), "f", "x.pdf");
        assert_ne!(p1, p2);
    }

    #[test]
    fn el_agente_sabe_donde_esta() {
        let a = Adjunto {
            file_id: "x".into(),
            nombre: "n.pdf".into(),
            tipo: "un archivo",
            caption: Some("revísalo".into()),
        };
        let t = mensaje(&a, Path::new("/tmp/n.pdf"));
        assert!(t.starts_with("revísalo\n\n[Julián mandó un archivo"));
        assert!(t.contains("/tmp/n.pdf"));
    }
}
