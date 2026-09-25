# Dependency Update Policy

Each registered project has an `update_policy`. It sets how far nightly
maintenance may move the project's dependencies. The policy is a ceiling.
Each level includes the levels below it.

| Policy | What maintenance may do |
| --- | --- |
| `patch` | Move the lockfile inside the existing manifest constraints. No manifest constraint changes. |
| `minor` | Also widen a constraint to the newest release that is not a breaking (major) release. |
| `major` | Also take major releases. A major upgrade never happens inside the nightly maintain session. Each one becomes its own `foundry task`. |

When a project has no policy, maintenance behaves as `minor`. The maintenance
summary flags the project with "no update policy set", so that you choose one.

Set the policy with the registry commands:

```bash
foundry registry edit my-tool --update-policy major
foundry registry add --name my-tool ... --update-policy patch
```

`foundry registry show my-tool` shows the policy on the `Updates:` line.
