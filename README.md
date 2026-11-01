
# Pareto

> Pareto frontier at fingertip

![](./image.png)

```
cargo build --bin pareto
sudo ./target/debug/pareto install
pareto --dict-data <PATH> # See hoverpanel
```

This project synthesizes 

- https://github.com/ple1n/hoverpanel/
- https://github.com/waycrate/exwlshelleventloop/tree/master/iced_examples/visibility
- https://crates.io/crates/metacomplete/ which enables the feature to instantly fuzzy-autocomplete search through millions of phrases 

`master` branch is production-ready.

- [x] Dictionary functionality. 
- [x] Word picking through mere selection, on Wayland
- [ ] File indexing
- [ ] Firefox bookmarks indexing