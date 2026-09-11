//! Signals during a real command's outstanding HTTP request.
use crate::common::{assert_ok, commit_all, gat, gat_bin, init_repo, isolated_child_env};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

#[test]
fn interrupted_transfers_exit_with_signal_status_and_hooks_stay_quiet() {
    for (args, signal, code, hook) in [
        (vec!["push"], "-INT", 130, false),
        (vec!["push"], "-TERM", 143, false),
        (
            vec!["hook", "post-checkout", "", "", "1"],
            "-INT",
            130,
            true,
        ),
    ] {
        let repo = init_repo();
        repo.write("asset.bin", "payload");
        assert_ok(&gat(repo.path(), &["add", "asset.bin"]), "add asset");
        commit_all(repo.path(), "asset");
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut url = url::Url::parse("s3://fixture").unwrap();
        url.query_pairs_mut()
            .append_pair(
                "endpoint",
                &format!("http://{}", listener.local_addr().unwrap()),
            )
            .append_pair("region", "fixture")
            .append_pair("skip_signature", "true");
        repo.write("gat.yaml", format!("remotes:\n  default: origin\n  origin:\n    url: '{url}'\nsync:\n  auto_fetch: true\n"));
        if hook {
            // Force the hook's fetch to need bytes, not just cached metadata.
            std::fs::remove_dir_all(repo.path().join(".gat/objects")).unwrap();
            std::fs::remove_file(repo.path().join("asset.bin")).unwrap();
        }
        let (ready, request) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            loop {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    headers.push(byte[0]);
                }
                let headers = String::from_utf8(headers).unwrap();
                if headers.lines().next().unwrap().contains("list-type=2") {
                    // Let the operation's readiness check succeed; the signal must
                    // interrupt a transfer request, not merely remote setup.
                    let body = "<ListBucketResult><Name>fixture</Name><IsTruncated>false</IsTruncated></ListBucketResult>";
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                    continue;
                }
                ready.send(headers).unwrap();
                // No response until the client cancels. EOF proves the command
                // dropped this request; there is no sleep or timed failure trigger.
                let mut byte = [0];
                assert_eq!(stream.read(&mut byte).unwrap(), 0);
                break;
            }
        });
        let mut command = Command::new(gat_bin());
        command
            .args(&args)
            .current_dir(repo.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        isolated_child_env(&mut command);
        let mut child = test_support::ChildGuard::spawn(command);
        let headers = request.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(
            headers.starts_with(if hook { "GET " } else { "HEAD " }),
            "{headers}"
        );
        assert!(
            Command::new("kill")
                .args([signal, &child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        server.join().unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(code));
        let mut stdout = String::new();
        child
            .take_stdout()
            .unwrap()
            .read_to_string(&mut stdout)
            .unwrap();
        let mut stderr = String::new();
        child
            .take_stderr()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        assert!(stdout.is_empty(), "{stdout}");
        if hook {
            assert!(stderr.is_empty(), "{stderr}");
        } else {
            assert!(stderr.contains("Interrupted"), "{stderr}");
        }
        assert!(!stderr.contains("complete"), "{stderr}");
    }
}
