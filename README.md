# bonsai

A small B-Tree engine built to understand how Postgres stores, indexes and recovers data.

Development shell: `nix develop`, or `direnv allow` once.

## Todo

- [ ] Foundations: relation files, a heap
- [ ] The tree: a B-tree index over the heap, and a buffer pool
- [ ] Transactions and durability: MVCC, then crash recovery
- [ ] Concurrency: concurrent readers and writers
- [ ] MVCC bookkeeping: VACUUM, free space and visibility maps
