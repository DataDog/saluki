//! HTTP requests sent from inside a Windows target container.
//!
//! The request runs in PowerShell with the .NET `HttpClient`, not with `curl.exe`. `curl.exe` comes from the image's
//! `System32`, so it can fail to start when the image's Windows build is newer than the host's. PowerShell and .NET
//! ship outside `System32` in the Datadog Agent images, so they do not have this problem.

use crate::core::probe::AttemptLimits;

/// One HTTP request sent from inside a Windows container.
pub(crate) struct WindowsHttpRequest<'a> {
    /// The HTTP method, for example `GET`.
    pub(crate) method: &'a str,
    /// The URL to send the request to.
    pub(crate) url: &'a str,
    /// Accept any server certificate.
    pub(crate) insecure_skip_verify: bool,
    /// Print the response body before the status.
    pub(crate) read_body: bool,
    /// A file in the container that holds a bearer token for the `Authorization` header.
    pub(crate) bearer_token_path: Option<&'a str>,
    /// A form-encoded request body.
    pub(crate) form_body: Option<&'a str>,
}

impl WindowsHttpRequest<'_> {
    /// Returns a `pwsh` command that sends the request, bounded by the request limit of `limits`.
    ///
    /// The command writes the status on the last line of its output. With `read_body`, the response body precedes the
    /// status. The command exits with a non-zero code when the request fails, for example on a refused connection or a
    /// timeout.
    pub(crate) fn command(&self, limits: &AttemptLimits) -> Vec<String> {
        // The script uses no double quotes, so Docker's command-line quoting can't change it.
        let mut statements = vec![
            "[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)".to_string(),
            "$handler = [System.Net.Http.HttpClientHandler]::new()".to_string(),
        ];
        if self.insecure_skip_verify {
            statements.push(
                "$handler.ServerCertificateCustomValidationCallback = \
                 [System.Net.Http.HttpClientHandler]::DangerousAcceptAnyServerCertificateValidator"
                    .to_string(),
            );
        }
        statements.push("$client = [System.Net.Http.HttpClient]::new($handler)".to_string());
        statements.push(format!(
            "$client.Timeout = [TimeSpan]::FromSeconds({})",
            limits.request_seconds()
        ));
        statements.push(format!(
            "$request = [System.Net.Http.HttpRequestMessage]::new([System.Net.Http.HttpMethod]::new({}), {})",
            single_quote(self.method),
            single_quote(self.url)
        ));
        if let Some(path) = self.bearer_token_path {
            statements.push(format!("$token = (Get-Content -Raw {}).Trim()", single_quote(path)));
            statements.push(
                "$null = $request.Headers.TryAddWithoutValidation('Authorization', 'Bearer ' + $token)".to_string(),
            );
        }
        if let Some(form) = self.form_body {
            statements.push(format!(
                "$request.Content = [System.Net.Http.StringContent]::new({}, [System.Text.Encoding]::UTF8, \
                 'application/x-www-form-urlencoded')",
                single_quote(form)
            ));
        }
        statements.push("$response = $client.SendAsync($request).GetAwaiter().GetResult()".to_string());
        if self.read_body {
            statements.push(
                "[Console]::Out.Write($response.Content.ReadAsStringAsync().GetAwaiter().GetResult())".to_string(),
            );
        }
        statements.push("[Console]::Out.Write([string][char]10 + [int]$response.StatusCode)".to_string());
        statements.push("exit 0".to_string());

        // On failure, write the message of each nested exception, so a timeout or refused connection names its cause
        // without a stack trace. An outer message often repeats an inner one, so skip the repeats.
        let script = format!(
            "$ErrorActionPreference = 'Stop'; try {{ {} }} catch {{ $e = $_.Exception; $m = $e.Message; \
             while ($e.InnerException) {{ $e = $e.InnerException; \
             if (-not $m.Contains($e.Message)) {{ $m += ' ' + $e.Message }} }}; \
             [Console]::Error.WriteLine($m); exit 1 }}",
            statements.join("; ")
        );
        ["pwsh", "-NoProfile", "-NonInteractive", "-Command", &script]
            .into_iter()
            .map(String::from)
            .collect()
    }
}

/// Returns `value` as a PowerShell single-quoted string literal.
fn single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{single_quote, WindowsHttpRequest};
    use crate::core::probe::AttemptLimits;

    fn get(url: &str) -> WindowsHttpRequest<'_> {
        WindowsHttpRequest {
            method: "GET",
            url,
            insecure_skip_verify: false,
            read_body: false,
            bearer_token_path: None,
            form_body: None,
        }
    }

    fn script(request: &WindowsHttpRequest<'_>) -> String {
        let command = request.command(&AttemptLimits::new(Duration::from_secs(10)));
        assert_eq!(command[..4], ["pwsh", "-NoProfile", "-NonInteractive", "-Command"]);
        command[4].clone()
    }

    #[test]
    fn the_request_does_not_use_curl() {
        let script = script(&get("http://127.0.0.1:5000/"));

        assert!(!script.contains("curl"), "{script}");
        assert!(script.contains("System.Net.Http.HttpClient"), "{script}");
    }

    #[test]
    fn the_request_gets_the_request_limit_of_the_attempt() {
        for remaining in [Duration::from_secs(60), Duration::from_secs(3)] {
            let limits = AttemptLimits::new(remaining);

            let command = get("http://127.0.0.1:5000/").command(&limits);

            let timeout = format!("[TimeSpan]::FromSeconds({})", limits.request_seconds());
            assert!(command[4].contains(&timeout), "{remaining:?}: {}", command[4]);
        }
    }

    #[test]
    fn the_script_has_no_double_quotes() {
        let request = WindowsHttpRequest {
            method: "POST",
            url: "https://127.0.0.1:5001/config/log_level",
            insecure_skip_verify: true,
            read_body: true,
            bearer_token_path: Some("C:\\ProgramData\\Datadog\\auth_token"),
            form_body: Some("value=debug"),
        };

        let script = script(&request);

        assert!(!script.contains('"'), "{script}");
    }

    #[test]
    fn options_add_only_their_own_parts() {
        let plain = script(&get("http://127.0.0.1:5000/"));
        let full = script(&WindowsHttpRequest {
            method: "POST",
            url: "https://127.0.0.1:5001/config/log_level",
            insecure_skip_verify: true,
            read_body: true,
            bearer_token_path: Some("C:\\ProgramData\\Datadog\\auth_token"),
            form_body: Some("value=debug"),
        });

        for part in [
            "DangerousAcceptAnyServerCertificateValidator",
            "ReadAsStringAsync",
            "'Bearer ' + $token",
            "'C:\\ProgramData\\Datadog\\auth_token'",
            "StringContent]::new('value=debug'",
        ] {
            assert!(!plain.contains(part), "{part}: {plain}");
            assert!(full.contains(part), "{part}: {full}");
        }
        assert!(full.contains("HttpMethod]::new('POST')"), "{full}");
    }

    #[test]
    fn single_quotes_are_doubled() {
        assert_eq!(single_quote("it's"), "'it''s'");
        assert_eq!(single_quote("plain"), "'plain'");
    }
}
