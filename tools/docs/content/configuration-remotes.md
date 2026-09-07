Each named remote has a `url`. `remotes.default` chooses the fallback by name;
{{command:remote add}} never selects it automatically.

```yaml
remotes:
  default: origin
  origin:
    url: file:///mnt/storage/project
```

<Accordion title="Remote URL examples">
  Choose a provider and replace its storage names. For authentication and
  supported URL options, see [Remote providers](/references/remote-providers).

  <Tabs sync={false}>
    <Tab title="S3">
      ```sh
      gat remote add origin 's3://example-bucket/project?region=eu-west-1'
      ```

      S3-compatible services also need an <Tooltip tip="The service’s HTTP(S) address, separate from the bucket name. This tells Gat where to send storage requests for an S3-compatible service." cta="S3-compatible setup" href="/references/remote-providers#s3-s3">endpoint</Tooltip>:

      ```sh
      gat remote add minio 's3://example-bucket/project?region=us-east-1&endpoint=http://localhost:9000'
      ```
    </Tab>
    <Tab title="Azure">
      ```sh
      gat remote add origin 'azblob://example-container/project?endpoint=https://myaccount.blob.core.windows.net'
      ```
    </Tab>
    <Tab title="GCS">
      ```sh
      gat remote add origin 'gcs://example-bucket/project'
      ```
    </Tab>
    <Tab title="OSS">
      ```sh
      gat remote add origin 'oss://example-bucket/project?endpoint=https://oss-cn-hangzhou.aliyuncs.com'
      ```
    </Tab>
    <Tab title="Local">
      <CodeGroup>
        ```sh macOS / Linux
        gat remote add origin 'file:///mnt/backup/project'
        ```

        ```powershell Windows PowerShell
        gat remote add origin 'file:///C:/gat-storage/project'
        ```
      </CodeGroup>
    </Tab>
  </Tabs>
</Accordion>

Use {{command:remote default}} to select a default and {{command:remote update}}
to change an existing URL. Explicit `--remote` overrides routes, which override
the default. See [routing precedence](/guides/using-multiple-remotes#remote-selection-at-a-glance)
for path-based storage, or [Set up a remote](/set-up-a-remote) for the full workflow.

<Note>
  Keep literal credentials out of committed URLs. Use provider credentials or
  single-quoted `${NAME}` references. Set referenced variables before validating
  or using the URL. See [URL templates](/references/remote-providers#use-environment-variables-in-a-url).
</Note>
