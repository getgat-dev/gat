Read or change general settings. Reads show the effective value and its source;
writes affect one scope, with project as the default.

| Task | How |
| --- | --- |
| Set a <Tooltip tip="One setting value, such as true, a number, or a strategy name. List settings instead take one argument for each element." cta="Find a setting’s type" href="/references/configuration">scalar</Tooltip> | Pass one value. |
| Replace a list | Pass one argument per element; commas are not separators. |
| Save an empty list | Use `--clear` where allowed. |
| Restore inheritance | Use `--unset`. |

Manage named resources with {{command:remote}}, {{command:mount}},
{{command:route}}, and {{command:selection}}. See
[Config inheritance](/concepts/config-inheritance) for scope rules and
[Configuration](/references/configuration) for keys and defaults.

<Note>
Output includes headings and source information. It is intended for people,
not as a serialized configuration format.
</Note>
