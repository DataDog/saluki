use std::fs::FileType;

use super::*;

pub struct DirCheckRequestListener {
    pub base_path: PathBuf,
    pub known_check_requests: Vec<CheckRequest>,

    // These could all be oneshot channels I think
    // but maybe buffering is useful
    pub new_path_tx: mpsc::Sender<CheckRequest>,
    pub deleted_path_tx: mpsc::Sender<CheckRequest>,
    pub new_path_rx: Option<mpsc::Receiver<CheckRequest>>,
    pub deleted_path_rx: Option<mpsc::Receiver<CheckRequest>>,
}

impl DirCheckRequestListener {
    /// Constructs a new `Listener` that will monitor the specified path.
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<DirCheckRequestListener, Error> {
        let path_ref = path.as_ref();
        if !path_ref.exists() {
            return Err(Error::DirectoryIncorrect {
                source: io::Error::new(io::ErrorKind::NotFound, "Path does not exist"),
            });
        }
        if !path_ref.is_dir() {
            return Err(Error::DirectoryIncorrect {
                source: io::Error::new(io::ErrorKind::NotFound, "Path is not a directory"),
            });
        }

        let (new_paths_tx, new_paths_rx) = mpsc::channel(100);
        let (deleted_paths_tx, deleted_paths_rx) = mpsc::channel(100);
        Ok(DirCheckRequestListener {
            base_path: path.as_ref().to_path_buf(),
            known_check_requests: Vec::new(),
            new_path_tx: new_paths_tx,
            deleted_path_tx: deleted_paths_tx,
            new_path_rx: Some(new_paths_rx),
            deleted_path_rx: Some(deleted_paths_rx),
        })
    }

    pub fn subscribe(&mut self) -> (mpsc::Receiver<CheckRequest>, mpsc::Receiver<CheckRequest>) {
        if self.new_path_rx.is_none() || self.deleted_path_rx.is_none() {
            panic!("Invariant violated: subscribe called after consuming the receivers");
        }

        (self.new_path_rx.take().unwrap(), self.deleted_path_rx.take().unwrap())
    }

    pub async fn update_check_entities(&mut self) -> Result<(), Error> {
        let new_check_paths = self.get_check_entities().await;
        let current_paths = self.known_check_requests.iter().map(|e| e.source.clone());
        let current: HashSet<CheckSource> = HashSet::from_iter(current_paths);
        let new: HashSet<CheckSource> = HashSet::from_iter(new_check_paths.iter().cloned());

        for entity in new.difference(&current) {
            let check_request = entity.to_check_request()?;
            self.known_check_requests.push(check_request.clone());
            // todo error handling
            self.new_path_tx.send(check_request).await.expect("Could send");
        }
        for entity in current.difference(&new) {
            // find the check request by path
            let check_request = self
                .known_check_requests
                .iter()
                .find(|e| e.source == *entity)
                .expect("couldn't find to-be-removed check")
                .clone();
            self.known_check_requests.retain(|e| e.source != *entity);
            // todo error handling
            self.deleted_path_tx.send(check_request).await.expect("Could send");
        }

        Ok(())
    }

    /// Retrieves all check entities from the base path that match the required check formats.
    pub async fn get_check_entities(&self) -> Vec<CheckSource> {
        let entries = WalkDir::new(&self.base_path).filter(|entry| async move {
            let file_type = entry.file_type().await.expect("Error getting file type");
            let mut path = entry.path();
            let mut extension = path.extension().unwrap_or_default();
            // JMX checks offer a `metrics.yaml`/`metrics.yml`
            // we don't care about these, ignore them.
            if path.to_string_lossy().ends_with("metrics.yaml") || path.to_string_lossy().ends_with("metrics.yml") {
                return Filtering::Ignore;
            }

            if file_type.is_file() && extension == "default" {
                // is default entry
                path.set_extension(""); // trim off '.default'
                extension = path.extension().unwrap_or_default(); // update extension
            }
            let is_d_dir = file_type.is_dir() && path.to_string_lossy().ends_with('d');
            let is_yaml_file = file_type.is_file() && (extension == "yaml" || extension == "yml");
            if is_d_dir || is_yaml_file {
                Filtering::Continue
            } else {
                Filtering::Ignore
            }
        });

        entries
            .filter_map(|e| async move {
                match e {
                    Ok(entry) => {
                        if entry.path().is_dir() {
                            return None;
                        }
                        match tokio::fs::read_to_string(entry.path()).await {
                            Ok(content) => Some(CheckSource::Yaml((entry.path(), content))),
                            Err(e) => {
                                eprintln!("Error reading file {}: {}", entry.path().display(), e);
                                None
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("Error traversing files: {}", e);
                        None
                    }
                }
            })
            .collect()
            .await
    }
}

pub struct DirCheckListenerContext {
    pub shutdown_handle: DynamicShutdownHandle,
    pub listener: DirCheckRequestListener,
    pub submit_runnable_check_req: mpsc::Sender<RunnableCheckRequest>,
    pub submit_stop_check_req: mpsc::Sender<CheckRequest>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    macro_rules! setup_test_dir {
        ($($file_path:expr => $content:expr),* $(,)?) => {{
            let temp_dir = tempfile::tempdir().expect("get tempdir");
            let temp_path = temp_dir.path();

            $(
                let path = temp_path.join($file_path);
                // This will create the parent path if needed
                let mut cloned = path.clone();
                if cloned.pop() {
                    tokio::fs::create_dir_all(&cloned).await.expect("dir create all");
                }
                let mut file = tokio::fs::File::create(&path).await.expect("file creation");
                file.write_all($content.as_bytes()).await.expect("file write");
            )*

            let listener = DirCheckRequestListener::from_path(&temp_path).unwrap();
            (listener, temp_path.to_path_buf(), temp_dir)
        }};
    }

    #[tokio::test]
    async fn test_get_check_entities_with_yaml_files() {
        // _temp_dir must stay alive till end of test
        let (listener, temp_path, _temp_dir) = setup_test_dir!(
            "file1.yaml" => "sample content",
            "file2.yml" => "sample content",
            "ignored.txt" => "",
            "dir.d/file3.yaml" => "sample content",
        );

        let check_sources = listener.get_check_entities().await;

        let expected_paths: Vec<CheckSource> = vec![
            CheckSource::Yaml((temp_path.join("file1.yaml"), "sample content".to_string())),
            CheckSource::Yaml((temp_path.join("file2.yml"), "sample content".to_string())),
            CheckSource::Yaml((temp_path.join("dir.d/file3.yaml"), "sample content".to_string())),
        ];

        assert_eq!(check_sources.len(), expected_paths.len());
        for source in check_sources {
            assert!(expected_paths.contains(&source));
        }
    }

    #[tokio::test]
    async fn test_get_check_entities_2() {
        // _temp_dir must stay alive till end of test
        let (listener, temp_path, _temp_dir) = setup_test_dir!(
            "myched.ck.yaml" => "sample content",
            "metrics.yaml" => "sample content",
            "metrics.yml" => "sample content",
            "file2yml" => "sample content",
            "mycheck.d/test.txt" => "sample content",
            "dir.d/file3.yml" => "sample content",
        );

        let check_sources = listener.get_check_entities().await;

        let expected_paths: Vec<CheckSource> = vec![
            CheckSource::Yaml((temp_path.join("myched.ck.yaml"), "sample content".to_string())),
            CheckSource::Yaml((temp_path.join("dir.d/file3.yml"), "sample content".to_string())),
        ];

        assert_eq!(check_sources.len(), expected_paths.len());
        for source in check_sources {
            assert!(expected_paths.contains(&source));
        }
    }

    #[tokio::test]
    async fn test_get_check_entities_default() {
        // _temp_dir must stay alive till end of test
        let (listener, temp_path, _temp_dir) = setup_test_dir!(
            "mycheck.yaml" => "sample content",
            "othercheck.yaml.default" => "sample content",
        );

        let check_sources = listener.get_check_entities().await;

        let expected_paths: Vec<CheckSource> = vec![
            CheckSource::Yaml((temp_path.join("mycheck.yaml"), "sample content".to_string())),
            CheckSource::Yaml((temp_path.join("othercheck.yaml.default"), "sample content".to_string())),
        ];

        assert_eq!(check_sources.len(), expected_paths.len());
        for source in check_sources {
            assert!(expected_paths.contains(&source));
        }
    }
}
