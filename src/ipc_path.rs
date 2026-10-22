//! 同机进程间的无损路径编码。正常路径仍用字符串;非 Unicode 路径保存
//! Unix 原始字节或 Windows UTF-16 码元,不经过 to_string_lossy。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum WirePath {
    Text(String),
    #[cfg(unix)]
    Unix {
        unix: Vec<u8>,
    },
    #[cfg(windows)]
    Windows {
        windows: Vec<u16>,
    },
}

impl WirePath {
    fn from_path(path: &Path) -> Self {
        if let Some(text) = path.to_str() {
            return Self::Text(text.to_owned());
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            Self::Unix {
                unix: path.as_os_str().as_bytes().to_vec(),
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            Self::Windows {
                windows: path.as_os_str().encode_wide().collect(),
            }
        }
    }

    fn into_path(self) -> PathBuf {
        match self {
            Self::Text(text) => text.into(),
            #[cfg(unix)]
            Self::Unix { unix } => {
                use std::os::unix::ffi::OsStringExt;
                std::ffi::OsString::from_vec(unix).into()
            }
            #[cfg(windows)]
            Self::Windows { windows } => {
                use std::os::windows::ffi::OsStringExt;
                std::ffi::OsString::from_wide(&windows).into()
            }
        }
    }
}

pub(crate) fn serialize<S: Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
    WirePath::from_path(path).serialize(serializer)
}

pub(crate) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
    Ok(WirePath::deserialize(deserializer)?.into_path())
}

pub(crate) mod list {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(
        paths: &[PathBuf],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(paths.iter().map(|path| WirePath::from_path(path)))
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<PathBuf>, D::Error> {
        Ok(Vec::<WirePath>::deserialize(deserializer)?
            .into_iter()
            .map(WirePath::into_path)
            .collect())
    }
}
