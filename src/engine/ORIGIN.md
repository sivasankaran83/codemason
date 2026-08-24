# Where this came from

Adopted source, not a vendored copy. It is ours now: reviewed, changed and
maintained like any other crate here, with no merge path back upstream.
ADR 0009 records why.

    Project   semble_rs
    Origin    https://github.com/johunsang/semble_rs
    Tag       v0.9.1
    Dated     15 May 2026
    Taken     11 August 2026
    Author    hunsang jo <johunsang@gmail.com>
    Licence   MIT

## About the licence file beside this one

Upstream declares MIT in `Cargo.toml` and in its README, and ships no licence
text and no copyright notice: there is no LICENSE or COPYING file anywhere in
the repository at v0.9.1.

MIT is a conditional grant, and the condition is that the notice travels with
the software. So `LICENSE` beside this file is the standard MIT text with the
copyright attributed to the author named in upstream's own `Cargo.toml`. It was
written here rather than copied, because there was nothing to copy.

That is the honest reading of an unambiguous declaration, and it is more
faithful than shipping the code with no notice at all. It is not a substitute
for upstream adding the file, which is worth asking for and does not block
anything here.

## What changed after it was taken

The lock file was updated on adoption. At v0.9.1 as shipped, `cargo deny`
reported a vulnerability in crossbeam-epoch and an unsoundness in anyhow, and a
plain `cargo update` resolved both. Owning the lock is the reason we could.

Anything else is in the git history of this directory, which is where it
belongs now.
