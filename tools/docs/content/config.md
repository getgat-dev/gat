Read or change general settings. Reads show the effective value and its source;
writes affect one scope, with project as the default.

| Task | How |
| --- | --- |
| Set a <Tooltip tip="One setting value, such as true, a number, or a strategy name. List settings instead take one argument for each element." cta="Find a setting’s type" href="/references/configuration">scalar</Tooltip> | Pass one value. |
| Replace a list | Pass one argument per element; commas are not separators. |
| Save an empty list | Use `--clear` where allowed. |
| Restore inheritance | Use `--unset`. |

General settings control cache placement, automatic fetching, and Git exclusions.
For <Tooltip tip="Named definitions with related fields, such as a remote URL or a mount's source and target. Their commands validate the definition and perform its associated operations." cta="Settings and resources" href="/concepts/config-inheritance#general-settings-and-managed-resources">managed resources</Tooltip>, use
{{command:remote}}, {{command:route}}, {{command:mount}}, or {{command:selection}}.
`gat config` does not read or write individual resource fields.

See [Config inheritance](/concepts/config-inheritance) for scope rules and
[Configuration](/references/configuration) for keys and defaults.

<Accordion title="A resource key was rejected">
  The error points to the command that manages the resource. For example,
  change `mounts.models.rev` with:

  ```sh
  gat mount update models --rev release
  ```

  Mount commands maintain the generated `rev_lock`; choose the desired revision
  through `rev`. See [pinning an upstream version](/guides/consuming-gat-assets#pin-the-upstream-version).
</Accordion>

<Note>
Output includes headings and source information. It is intended for people,
not as a serialized configuration format.
</Note>

Environment overrides use `GAT_` followed by the uppercase setting path, replacing
each dot with `_`. For example, `network.request_concurrency` becomes
`GAT_NETWORK_REQUEST_CONCURRENCY`. The configuration reference lists every
supported name. Overrides apply above global, project, and local files for one
invocation; explicit command options take precedence where available.

```sh
export GAT_NETWORK_REQUEST_CONCURRENCY=64
export GAT_NETWORK_READINESS_TIMEOUT_SECONDS=15
export GAT_GIT_IGNORE_PATTERNS='["*.bin", "artifacts,legacy/**", "space name/**"]'
export GAT_CACHE_MATERIALIZATION_STRATEGY='["reflink", "copy"]'
```

List overrides are JSON arrays of strings. Commas and spaces inside strings are
preserved. `[]` explicitly empties an allowed list; an empty environment string
is invalid. Remove a variable to restore file inheritance. Boolean values must
be `true` or `false`. Timeouts accept whole seconds from 1 through 86400;
concurrency accepts counts from 1 through 65535.

Only general settings accept overrides. Named remotes, routes, mounts, selections,
and default remote/selection choices remain file configuration managed by their
commands. `${NAME}` in a remote URL is explicit template interpolation, not a
resource override. Gat captures template variables once per invocation too.

`gat config` writes only the selected file scope. It reports when an environment
variable or higher file scope still overrides the saved value. Unknown environment
names are ignored; invalid supported overrides fail before repository work.
`GAT_CACHE_DIR` and `GAT_CONNECT_TIMEOUT` have been removed; use
`GAT_CACHE_LOCATION` and `GAT_NETWORK_READINESS_TIMEOUT_SECONDS`.
