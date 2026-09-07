Manage named storage locations for large-file objects. Reads show the <Tooltip tip="The result of combining global, project, and local settings. A remote can be inherited even when it is absent from the file you are editing." cta="Understand configuration layers" href="/concepts/config-inheritance">effective configuration</Tooltip>; changes use project scope unless you pass `--global` or `--local`.

<Note>
  Adding a remote does not choose a default. Run `gat remote default NAME`, or
  use `--remote NAME` for one transfer. Gat remotes are separate from Git remotes.
</Note>

Updates and removals must target the defining scope. To override a project
remote on one machine, add the same name with `--local`.

<Accordion title="URL templates and storage options">
  Gat supports `file://`, `s3://`, `azblob://`, `gcs://`, and `oss://` when
  enabled in the build. Use single-quoted `${VAR}` references for URL secrets.
  Read-only commands redact sensitive values without expanding templates or
  contacting storage. Redaction does not remove secrets from the saved file.

  File remotes accept only the optional `root` query setting. Uploads publish
  by atomic rename and may replace an existing object at the same key.

  See [Remote providers](/references/remote-providers) for authentication and
  [Set up a remote](/set-up-a-remote) for a complete workflow.
</Accordion>
