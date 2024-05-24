use tokio::fs;

use super::*;

pub struct DirCheckRequestListener {
    pub search_paths: Vec<PathBuf>,
    pub known_check_requests: Vec<CheckRequest>,

    // These could all be oneshot channels I think
    // but maybe buffering is useful
    pub new_path_tx: mpsc::Sender<CheckRequest>,
    pub deleted_path_tx: mpsc::Sender<CheckRequest>,
    pub new_path_rx: Option<mpsc::Receiver<CheckRequest>>,
    pub deleted_path_rx: Option<mpsc::Receiver<CheckRequest>>,
}

impl DirCheckRequestListener {
    /// Constructs a new `Listener` that will monitor the specified paths.
    pub fn from_paths<P: AsRef<Path>>(paths: Vec<P>) -> Result<DirCheckRequestListener, Error> {
        let search_paths: Vec<PathBuf> = paths
            .iter()
            .filter_map(|p| {
                if !p.as_ref().exists() {
                    warn!("Skipping path '{}' as it does not exist", p.as_ref().display());
                    return None;
                }
                if !p.as_ref().is_dir() {
                    warn!("Skipping path '{}', it is not a directory.", p.as_ref().display());
                    return None;
                }
                Some(p.as_ref().to_path_buf())
            })
            .collect();

        let (new_paths_tx, new_paths_rx) = mpsc::channel(100);
        let (deleted_paths_tx, deleted_paths_rx) = mpsc::channel(100);
        Ok(DirCheckRequestListener {
            search_paths,
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

    pub async fn update_check_entities(&mut self) -> Result<(), GenericError> {
        let new_check_paths = self.get_check_entities().await?;
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

    async fn consider_file(&self, original_path: PathBuf, provided_check_name: Option<String>) -> Option<CheckSource> {
        let mut path = original_path.clone();
        let mut extension = path.extension().unwrap_or_default().to_owned();
        if extension == "default" {
            // is default entry
            path.set_extension(""); // trim off '.default'
            extension = path.extension().unwrap_or_default().to_owned()
        }
        if path.ends_with("metrics.yaml") || path.ends_with("metrics.yml") {
            // 'metrics.yaml' is for JMX checks. we don't care about them here
            return None;
        }
        if extension == "yaml" || extension == "yml" {
            // is yaml file
            let check_name: String = provided_check_name
                .unwrap_or_else(|| path.file_stem().unwrap_or_default().to_string_lossy().to_string());
            match fs::read_to_string(&original_path).await {
                Ok(contents) => {
                    let check = YamlCheck::new(check_name, contents, Some(original_path.to_path_buf()));
                    Some(CheckSource::Yaml(check))
                }
                Err(e) => {
                    eprintln!("Error reading file {}: {}", original_path.display(), e);
                    None
                }
            }
        } else {
            None
        }
    }
    /// Retrieves all check entities from the base path that match the required check formats.
    pub async fn get_check_entities(&self) -> Result<Vec<CheckSource>, GenericError> {
        let mut sources = vec![];
        for p in self.search_paths.iter() {
            let mut entries = fs::read_dir(p).await?;
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                let extension = path.extension().unwrap_or_default();
                let file_type = entry.file_type().await?;
                if file_type.is_dir() && extension == "d" {
                    let module_name = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
                    let mut sub_entries = fs::read_dir(path).await?;
                    while let Ok(Some(sub_entry)) = sub_entries.next_entry().await {
                        let path = sub_entry.path();
                        if let Some(source) = self.consider_file(path, Some(module_name.clone())).await {
                            sources.push(source);
                        }
                    }
                } else if file_type.is_file() {
                    if let Some(source) = self.consider_file(path, None).await {
                        sources.push(source);
                    }
                }
            }
        }
        Ok(sources)
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

            let listener = DirCheckRequestListener::from_paths(vec![&temp_path]).unwrap();
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

        let mut check_sources = listener.get_check_entities().await.unwrap();

        let mut expected_sources: Vec<CheckSource> = vec![
            CheckSource::Yaml(YamlCheck::new(
                "file1",
                "sample content",
                Some(temp_path.join("file1.yaml")),
            )),
            CheckSource::Yaml(YamlCheck::new(
                "file2",
                "sample content",
                Some(temp_path.join("file2.yml")),
            )),
            CheckSource::Yaml(YamlCheck::new(
                "dir",
                "sample content",
                Some(temp_path.join("dir.d/file3.yaml")),
            )),
        ];

        check_sources.sort();
        expected_sources.sort();

        for (idx, source) in check_sources.iter().enumerate() {
            assert_eq!(source, &expected_sources[idx]);
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

        let mut check_sources = listener.get_check_entities().await.unwrap();

        let mut expected_sources: Vec<CheckSource> = vec![
            CheckSource::Yaml(YamlCheck::new(
                "myched.ck",
                "sample content",
                Some(temp_path.join("myched.ck.yaml")),
            )),
            CheckSource::Yaml(YamlCheck::new(
                "dir",
                "sample content",
                Some(temp_path.join("dir.d/file3.yml")),
            )),
        ];

        check_sources.sort();
        expected_sources.sort();

        for (idx, source) in check_sources.iter().enumerate() {
            assert_eq!(source, &expected_sources[idx]);
        }
    }

    #[tokio::test]
    async fn test_get_check_entities_default() {
        // _temp_dir must stay alive till end of test
        let (listener, temp_path, _temp_dir) = setup_test_dir!(
            "mycheck.yaml" => "sample content",
            "othercheck.yaml.default" => "sample content",
        );

        let mut check_sources = listener.get_check_entities().await.unwrap();

        let mut expected_sources: Vec<CheckSource> = vec![
            CheckSource::Yaml(YamlCheck::new(
                "mycheck",
                "sample content",
                Some(temp_path.join("mycheck.yaml")),
            )),
            CheckSource::Yaml(YamlCheck::new(
                "othercheck",
                "sample content",
                Some(temp_path.join("othercheck.yaml.default")),
            )),
        ];

        check_sources.sort();
        expected_sources.sort();

        for (idx, source) in check_sources.iter().enumerate() {
            assert_eq!(source, &expected_sources[idx]);
        }
    }
}
