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
