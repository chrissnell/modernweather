# `breached-passwords.txt`

The list `mw_api_mgmt::auth::breached` looks a proposed password up in, embedded with
`include_str!`. **O-10 / W5 OQ-W5-2**, answered `(b)`: ship a list and warn on a hit, never
block on one, and never call an online service.

## What is in it

10,000 entries, 137 KB, one per line, **sorted bytewise** and looked up by binary search.
Every entry is lowercase ASCII and **at least 12 characters long**.

Those two properties are not cosmetic. The lookup lowercases the candidate before searching,
so one stored entry covers `password1234`, `Password1234` and `PASSWORD1234`. And a password
below 12 characters is already refused by W5 §3.5's minimum, so a shorter entry could never
be reached: of the 10,001 lines in the list the decision was written against
(`SecLists/Passwords/Common-Credentials/10k-most-common.txt`) exactly **ten** are 12
characters or longer, which would have put 72 KB in the binary to warn about ten passwords.
See `docs/reconciliation.md` **A-M7-2-2**.

## Where it came from

`SecLists/Passwords/Common-Credentials/xato-net-10-million-passwords-1000000.txt` — Mark
Burnett's corpus, frequency-ordered, redistributed by SecLists (MIT). Derived with:

```sh
awk 'length($0) >= 12 && length($0) <= 256' xato-net-10-million-passwords-1000000.txt \
  | tr 'A-Z' 'a-z' \
  | awk '!seen[$0]++' \
  | head -10000 \
  | LC_ALL=C sort > breached-passwords.txt
```

Frequency order is what `head` selects on, so these are the 10,000 most common passwords
that the length minimum would otherwise have let through; `LC_ALL=C sort` is last so the
committed file is in the order the binary search needs. `auth::tests` re-asserts both
invariants — sorted, and every entry at least 12 bytes — so a replacement list that breaks
either is a test failure rather than a lookup that silently returns `false`.

## Replacing it

Drop in a file with the same two invariants. No code changes, and the 256-byte upper bound
above matches the maximum password length `auth::POLICY_MAX_BYTES` accepts, so an entry
longer than that would be unreachable for the same reason a short one is.
