//! Where the correctness tools run on each platform: the executables `datadog-intake` and `millstone`, which one tools
//! image carries on each container OS and which macOS runs as host programs.

use crate::core::participant::{Enclosure, SpecContext};
use crate::core::spec::{ParticipantSpec, Program, DEFAULT_PROCESS};
use crate::core::template::Template;

/// The Linux tools image the `Makefile` builds, and the directory of the executables in it.
pub(super) const LINUX_IMAGE: &str = "saluki-images/correctness-tools:latest";
const LINUX_DIR: &str = "/usr/local/bin/";

/// The Windows tools image the Windows integration script builds, and the directory of the executables in it.
const WINDOWS_IMAGE: &str = "saluki-images/correctness-tools:testing-windows";
const WINDOWS_DIR: &str = "C:\\tools\\";

/// The directory of the host programs `make build-correctness-tools-host` builds. A run replaces each with `--set`.
const HOST_DIR: &str = "target/release/";

/// Fills in what runs the executable `binary` with `args`, as the participant's only process: an image and its command
/// in a container or pod, and a host program on macOS.
///
/// # Errors
///
/// Returns an error if the realization has no tools, or an argument is not a valid template.
pub(super) fn launch(cx: &SpecContext<'_>, binary: &str, args: &[&str]) -> Result<ParticipantSpec, String> {
    let args = args
        .iter()
        .copied()
        .map(Template::parse)
        .collect::<Result<Vec<_>, _>>()?;
    let in_image = |image: &str, path: String| {
        let command = std::iter::once(Template::parse(&path)).chain(args.iter().cloned().map(Ok));
        Ok(ParticipantSpec {
            image: Some(image.to_string()),
            command: Some(command.collect::<Result<_, _>>()?),
            ..ParticipantSpec::default()
        })
    };
    match cx.realization.enclosure {
        Enclosure::LinuxContainer | Enclosure::KubernetesPod => {
            in_image(LINUX_IMAGE, format!("{}{}", LINUX_DIR, binary))
        }
        Enclosure::WindowsContainer => in_image(WINDOWS_IMAGE, format!("{}{}.exe", WINDOWS_DIR, binary)),
        Enclosure::HostProcessGroup => Ok(ParticipantSpec {
            programs: Some(vec![Program {
                name: DEFAULT_PROCESS.to_string(),
                path: format!("{}{}", HOST_DIR, binary),
                args,
                env: Default::default(),
            }]),
            ..ParticipantSpec::default()
        }),
    }
}
