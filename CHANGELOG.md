## 0.2.0 - 2024-11-28

### Added

* Add `Repo::peer_state` which returns information about how in-sync we are 
  with another peer. (`469a3556`)

### Fixed

* Fix an issue where the Repo could fail to respond to sync messages for a 
  document which is authorized. (`a5b19f79`)


