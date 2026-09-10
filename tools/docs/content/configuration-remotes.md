Each named remote has a `url`. `remotes.default` chooses the fallback by name;
{{command:remote add}} never selects it automatically.

```yaml
remotes:
  default: origin
  origin:
    url: file:///mnt/storage/project
```

Choose a provider below for URL examples and authentication:

| Storage | URL scheme | Setup |
| --- | --- | --- |
| Amazon S3 or compatible service | `s3://` | [Region, credentials, and custom endpoints](/references/remote-providers#s3-s3) |
| Azure Blob | `azblob://` | [Account endpoint and credentials](/references/remote-providers#azure-blob-azblob) |
| Google Cloud Storage | `gcs://` | [Application Default Credentials](/references/remote-providers#google-cloud-storage-gcs) |
| Alibaba OSS | `oss://` | [Regional endpoint and credentials](/references/remote-providers#alibaba-cloud-oss-oss) |
| Local or mounted directory | `file://` | [Absolute paths on Unix and Windows](/references/remote-providers#local-directory-file) |

Use {{command:remote default}} to select a default and {{command:remote update}}
to change an existing URL. Explicit `--remote` overrides routes, which override
the default. See [routing precedence](/guides/using-multiple-remotes#remote-selection-at-a-glance)
for path-based storage, or [Set up a remote](/set-up-a-remote) for the full workflow.

<Note>
  Keep literal credentials out of committed URLs. Use provider credentials or
  single-quoted `${NAME}` references. Set referenced variables before validating
  or using the URL. See [URL templates](/references/remote-providers#use-environment-variables-in-a-url).
</Note>
