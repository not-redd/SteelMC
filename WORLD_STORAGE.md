# Steel World Storage Specification

This is the specification of how Steel stores a world on disk. Use it to write a tool that produces Steel worlds, such as a vanilla → Steel converter. Every rule links to the code that implements it.

- **Target:** Steel `0.17.0+mc26.3` ([Cargo.toml:31](Cargo.toml#L31)). Region format v23 ([format.rs:62](steel-core/src/chunk_saver/format.rs#L62)). Player storage v10 ([player_data_storage/mod.rs:41](steel-core/src/player/player_data_storage/mod.rs#L41)).
- **Vanilla references** link to the local 26.3 source in `minecraft-src/`. Wiki pages are listed in [§10](#10-references).

- **Input:** a vanilla 26.3 save, `DataVersion` 5023 ([SharedConstants.java:16](minecraft-src/minecraft/src/net/minecraft/SharedConstants.java#L16)). Steel has no data fixers, so upgrade older worlds with vanilla first (`--forceUpgrade`, [Main.java:76](minecraft-src/minecraft/src/net/minecraft/server/Main.java#L76)).
- **Dimensions:** a dimension converts only if `config/worlds.toml` has a matching world. Steel has no data-pack dimensions.

Stop the server before writing. Steel takes no lock on the save folder, and a running server keeps region headers in memory and writes them back later ([region_manager.rs:67](steel-core/src/chunk_saver/region_manager.rs#L67), [region_manager.rs:587](steel-core/src/chunk_saver/region_manager.rs#L587)).

---

## 0. Encoding rules used everywhere

### 0.1 wincode

Every Steel binary payload is [`wincode`](https://crates.io/crates/wincode) 0.6.3 with its default config ([Cargo.toml:60](Cargo.toml#L60)). Paths below starting with `wincode-0.6.3/` are in the crate source under `~/.cargo/registry/src/*/`.

| Rust type | Bytes | Source |
|---|---|---|
| `u8`, `i8` | 1 raw byte | `wincode-0.6.3/src/schema/impls.rs:250-299` |
| `u16` … `i64`, `u32`, `i32` | fixed width, little-endian (no varints) | `wincode-0.6.3/src/config/mod.rs:30-58`, `src/schema/int_encoding.rs:68-99` |
| `f32`, `f64` | IEEE bits, little-endian | `wincode-0.6.3/src/schema/impls.rs:129-187` |
| `bool` | 1 byte, `0` or `1`. Any other value is a decode error | `wincode-0.6.3/src/schema/impls.rs:301-345` |
| `Vec<T>`, `Box<[T]>` | `u64` LE element count, then the elements | `wincode-0.6.3/src/len.rs:206-245, 412-413` |
| `String`, `&str` | `u64` LE **byte** length, then UTF-8 | `wincode-0.6.3/src/schema/impls.rs:1045-1076` |
| `[T; N]` | N elements, **no** length prefix | `wincode-0.6.3/src/schema/impls.rs:636-700` |
| `(A, B)` | A then B, no prefix | `wincode-0.6.3/codegen/tuple.rs:33-37` |
| `Option<T>` | `u8` tag: `0` = None, `1` = Some followed by T | `wincode-0.6.3/src/schema/impls.rs:728-772` |
| derived `struct` | fields in declaration order, no padding | `wincode-derive-0.5.1/src/schema_write.rs:44-55` |
| derived `enum` | `u32` LE variant index (declaration order), then the variant's fields | `wincode-derive-0.5.1/src/common.rs:410-431` |

**Size cap.** Every sequence (`Vec<T>`, `Box<[T]>`, `String`) must satisfy `len × size_of::<T>() ≤ 4 MiB`, where `size_of::<T>()` is the size of one element as a Rust value, not its encoded size. This covers per-chunk lists such as `entities`, not only byte vectors. wincode checks it on both write and read (`wincode-0.6.3/src/config/mod.rs:30`, `src/len.rs:101-153`). Steel's own writer refuses to save such a chunk. A hand-written encoder that ignores the cap produces a chunk Steel can't decode, which is then regenerated (§0.4).

### 0.2 Steel types inside wincode

| Type | Encoding | Source |
|---|---|---|
| `Identifier` | one string `"namespace:path"` (u64 length + UTF-8). On read a missing namespace means `minecraft`. Namespace allows `[a-z0-9_.-]`, path also allows `/`. An invalid string fails the decode | [identifier.rs:131-140](steel-utils/src/types/identifier.rs#L131), read at [identifier.rs:203](steel-utils/src/types/identifier.rs#L203) |
| `PackedChunkPos` | `i64` LE = `(x & 0xFFFF_FFFF) \| ((z & 0xFFFF_FFFF) << 32)`, the same as vanilla `ChunkPos.pack` | [packed_position.rs:15-45](steel-utils/src/types/packed_position.rs#L15) |
| `ChunkStatus` | derived enum, `u32` LE index (see §4.1) | [status.rs:9](steel-core/src/chunk/status.rs#L9) |
| UUID `[u8; 16]` | RFC 4122 big-endian bytes (`Uuid::as_bytes`). Vanilla `int[4]` UUID = BE(i0)‖BE(i1)‖BE(i2)‖BE(i3) | [uuid_ext.rs:23-45](steel-utils/src/uuid_ext.rs#L23) |

### 0.3 NBT byte vectors ("compound body")

Every NBT stored inside a Steel file is a **compound body**, the form `simdnbt::owned::NbtCompound::write` writes (simdnbt 0.10.0, `src/owned/compound.rs:103-120`):

```text
for each entry:  [tag id: u8][name length: u16 BE][name: modified UTF-8][payload, big-endian Java NBT]
then:            0x00   (TAG_End)
```

There is **no** root type byte (`0x0A`) and **no** root name. To convert a vanilla compound, write its entries followed by `0x00`. An empty compound is the single byte `[0x00]`. Steel reads it with `simdnbt::borrow::read_compound`, which expects exactly this form ([storage/mod.rs:26](steel-core/src/chunk_saver/storage/mod.rs#L26)).

### 0.4 Rejected chunks are regenerated

When a chunk payload fails zstd, wincode, or any validation rule in this document, Steel **deletes that chunk's entry and generates the chunk again** ([region_manager.rs:505-523](steel-core/src/chunk_saver/region_manager.rs#L505), [region_manager.rs:528-543](steel-core/src/chunk_saver/region_manager.rs#L528)). Nothing is reported to the user beyond a log line. A converter must therefore get every rule in §4 right, or converted terrain is silently replaced.

---

## 1. Directory layout

Everything lives under `save_path` from `config/worlds.toml` ([worlds.toml:5](config/worlds.toml#L5), default `saves` from [config.rs:727](steel-core/src/config.rs#L727)). It is used as given ([config.rs:445](steel-core/src/config.rs#L445)): a relative path is relative to the server's working directory, and an absolute path also works.

```text
saves/
├── global/
│   ├── player_permissions.toml          # per-player permission groups/allow/deny
│   └── players/data/<uuid>              # STLG: last active domain
│
└── <domain>/                            # e.g. "minecraft"
    ├── players/
    │   ├── data/<uuid>                  # STLP: inventory, position, health, ...
    │   ├── stats/<uuid>.toml            # statistics
    │   └── advancements/<uuid>.json     # advancement progress
    │
    └── worlds/<world>/                  # e.g. "overworld", "the_nether", "the_end"
        ├── level.toml                   # seed, time, weather, spawn, border, game rules
        ├── region/r.<x>.<z>.srg         # chunks, 32×32 per file
        └── data/
            ├── chunk_tickets.toml       # every world
            ├── scoreboard.toml          # domain default world only
            ├── command_storage.toml     # domain default world only
            └── random_sequences.toml    # domain default world only
```

`<uuid>` is the lowercase hyphenated UUID. Player data files have no extension.

| Path | Built at |
|---|---|
| `<domain>/worlds/<world>/` | [registry.rs:80](steel-core/src/chunk_saver/registry.rs#L80) |
| `region/` | [registry.rs:168](steel-core/src/chunk_saver/registry.rs#L168) |
| `r.<x>.<z>.srg` | [format.rs:1195](steel-core/src/chunk_saver/format.rs#L1195) |
| `level.toml` | [level_data/mod.rs:423](steel-core/src/level_data/mod.rs#L423) |
| `data/<name>.toml` | [saved_data.rs:117](steel-utils/src/saved_data.rs#L117), [saved_data.rs:57](steel-utils/src/saved_data.rs#L57) |
| `global/…` | [file_storage.rs:332-342](steel-core/src/player/player_data_storage/file_storage.rs#L332) |
| `<domain>/players/…` | [file_storage.rs:348-362](steel-core/src/player/player_data_storage/file_storage.rs#L348) |

### 1.1 Domains and worlds

- A world's key is `<domain>:<world>`, and its domain is the key's namespace ([world/mod.rs:511](steel-core/src/world/mod.rs#L511)). The vanilla dimensions live in domain `minecraft` as worlds `overworld`, `the_nether`, `the_end` ([worlds.toml](config/worlds.toml)).
- One world per domain is the **default world**. Its files hold the data the domain shares: `game_time` (§5.2), `scoreboard.toml`, `command_storage.toml`, and `random_sequences.toml` (§6).
- Every world has its own `level.toml`, with its own seed, weather, border, clocks, and game rules. Vanilla keeps several of these server-wide (§8.1), so a converter copies them into each world. Steel reads each world's own copy.

### 1.2 File type tags

Each binary file starts with a 4-byte tag, `STL` (Steel) + one letter. Only `STLR` has its meaning written in the code; the others come from their constant names.

| Tag | Meaning | File | Source |
|---|---|---|---|
| `STLR` | Steel Region | `region/*.srg` | [format.rs:37-38](steel-core/src/chunk_saver/format.rs#L37) |
| `STLP` | Steel Player | `<domain>/players/data/<uuid>` | `PLAYER_MAGIC`, [player_data_storage/mod.rs:39](steel-core/src/player/player_data_storage/mod.rs#L39) |
| `STLG` | Steel Global | `global/players/data/<uuid>` | `GLOBAL_MAGIC`, [player_data_storage/mod.rs:40](steel-core/src/player/player_data_storage/mod.rs#L40) |

A file with the wrong tag is rejected ([region_manager.rs:118](steel-core/src/chunk_saver/region_manager.rs#L118), [player_data_storage/mod.rs:573](steel-core/src/player/player_data_storage/mod.rs#L573)).

---

## 2. Storage configuration

Worlds use the `steel:disk` backend ([registry.rs:153](steel-core/src/chunk_saver/registry.rs#L153)), which is the default when `worlds.toml` sets no `storage` ([config.rs:404](steel-core/src/config.rs#L404)). The setting is inherited root → domain → world: [config.rs:404](steel-core/src/config.rs#L404), [config.rs:470](steel-core/src/config.rs#L470), [config.rs:543](steel-core/src/config.rs#L543).

`steel:disk` takes one optional setting, `config = { path = "..." }`, relative to `save_path`. The world then lives at `<path>/` with `<path>/region/` and `<path>/level.toml` ([registry.rs:153-172](steel-core/src/chunk_saver/registry.rs#L153)):

```toml
[[domains.minecraft.worlds]]
name = "overworld"
generator = "minecraft:overworld"
default = true
storage = { type = "steel:disk", config = { path = "imported/overworld" } }
```

---

## 3. Region files (`.srg`)

### 3.1 Coordinates

- Region `(rx, rz) = (chunk_x.div_euclid(32), chunk_z.div_euclid(32))` ([format.rs:1177](steel-core/src/chunk_saver/format.rs#L1177)). File name `r.<rx>.<rz>.srg` ([format.rs:1195](steel-core/src/chunk_saver/format.rs#L1195)).
- Local position = `chunk.rem_euclid(32)` ([format.rs:1186](steel-core/src/chunk_saver/format.rs#L1186)). Table index = `local_z * 32 + local_x` ([format.rs:214](steel-core/src/chunk_saver/format.rs#L214)).

### 3.2 Byte layout

```text
offset 0      magic "STLR" (4) | version u16 LE (2) | padding (2)          8 bytes
offset 8      chunk table: 1024 entries × 8 bytes                          8192 bytes
                entry = [sector_offset u32 LE][size u24 LE][status u8]
offset 8200   zero padding up to sector 3
offset 12288  chunk payloads, each starting on a 4096-byte sector, zero-padded to the next sector
```

| Constant | Value | Source |
|---|---|---|
| `REGION_MAGIC` | `"STLR"` | [format.rs:38](steel-core/src/chunk_saver/format.rs#L38) |
| `FORMAT_VERSION` | `23` | [format.rs:62](steel-core/src/chunk_saver/format.rs#L62) |
| `SECTOR_SIZE` | 4096 | [format.rs:83](steel-core/src/chunk_saver/format.rs#L83) |
| `FIRST_DATA_SECTOR` | 3 | [format.rs:96](steel-core/src/chunk_saver/format.rs#L96) |
| `MAX_CHUNK_SIZE` | 16 MiB (compressed) | [format.rs:99](steel-core/src/chunk_saver/format.rs#L99) |

Chunk table entry ([format.rs:108](steel-core/src/chunk_saver/format.rs#L108), bytes in [format.rs:157](steel-core/src/chunk_saver/format.rs#L157)):

- `sector_offset`: payload start in sectors (byte offset = `sector_offset × 4096`). `0` means the chunk does not exist.
- `size`: exact compressed payload length in bytes (not sectors).
- `status`: `ChunkStatus` index of the payload (§4.1).

### 3.3 Header rules

A region file is rejected as a whole if any of these fail ([region_manager.rs:219-273](steel-core/src/chunk_saver/region_manager.rs#L219)):

- every present entry has `sector_offset ≥ 3`
- `1 ≤ size ≤ 16 MiB`
- `sector_offset + ceil(size / 4096)` is within the file
- no two entries overlap
- the status byte of every present entry is a valid index (0–9); absent entries are read as `Empty` ([format.rs:175-182](steel-core/src/chunk_saver/format.rs#L175), checked at [region_manager.rs:142-147](steel-core/src/chunk_saver/region_manager.rs#L142))

A file whose version is not `23` is renamed to `r.x.z.srg.v<N>.bak` and replaced by an empty region ([region_manager.rs:126-137](steel-core/src/chunk_saver/region_manager.rs#L126)).

### 3.4 Writing region files

The simplest correct writer is Steel's own: `RegionManager::new(region_dir)`, then `save_chunk_data(PreparedChunkSave { pos, status, persistent, handled_runtime_entity_ids: vec![] }, &pool)` per chunk, then `close_all()` ([region_manager.rs:86](steel-core/src/chunk_saver/region_manager.rs#L86), [region_manager.rs:333](steel-core/src/chunk_saver/region_manager.rs#L333), [region_manager.rs:668](steel-core/src/chunk_saver/region_manager.rs#L668), [`PreparedChunkSave`, region_manager.rs:55](steel-core/src/chunk_saver/region_manager.rs#L55)). It does the encoding, compression, sector allocation, and header writing.

`PreparedChunkSave.persistent` is a `PersistentChunk<'static>`, so block-state property names and values must be `&'static str`. Steel's registry property strings are static ([registry `blocks/mod.rs:599-626`](steel-registry/src/blocks/mod.rs#L599)).

---

## 4. Chunk payload

Payload bytes = `zstd(level 3, wincode(PersistentChunk))` ([region_manager.rs:410-413](steel-core/src/chunk_saver/region_manager.rs#L410)). Any zstd level decodes. The chunk does **not** store `min_y` or `height`; they come from the world's dimension type at load ([storage/mod.rs:471-483](steel-core/src/chunk_saver/storage/mod.rs#L471)).

`PersistentChunk` ([format.rs:364-393](steel-core/src/chunk_saver/format.rs#L364)), in wire order:

| # | Field | Type | Section |
|---|---|---|---|
| 1 | `last_modified` | `u32` Unix seconds. Never read | [storage/mod.rs:793](steel-core/src/chunk_saver/storage/mod.rs#L793) |
| 2 | `block_states` | `Vec<PersistentBlockState>` | §4.2 |
| 3 | `biomes` | `Vec<Identifier>` | §4.2 |
| 4 | `sections` | `Vec<PersistentSection>` | §4.2 |
| 5 | `block_entities` | `Vec<PersistentBlockEntity>` | §4.5 |
| 6 | `entities` | `Vec<PersistentEntity>` | §4.6 |
| 7 | `block_ticks` | `Vec<PersistentTick>` | §4.7 |
| 8 | `fluid_ticks` | `Vec<PersistentTick>` | §4.7 |
| 9 | `heightmaps` | `Vec<PersistentHeightmap>` | §4.3 |
| 10 | `light` | `PersistentLightData` | §4.4 |
| 11 | `postprocessing` | `Vec<Vec<u16>>` | §4.8 |
| 12 | `structure_starts` | `Vec<PersistentStructureStart>` | §4.9 |
| 13 | `structure_references` | `Vec<PersistentStructureReference>` | §4.9 |
| 14 | `pois` | `Vec<PersistentPoi>` | §4.10 |

### 4.1 Status

The status is stored in the region header byte (§3.2), not in the payload. Steel's statuses are the same as vanilla 26.3's, in the same order ([status.rs:9-30](steel-core/src/chunk/status.rs#L9), [ChunkStatus.java:21-30](minecraft-src/minecraft/src/net/minecraft/world/level/chunk/status/ChunkStatus.java#L21)):

| Index | Status | Heightmaps kept | Notes |
|---|---|---|---|
| 0 | `Empty` | 4, 5 | never saved by Steel (see below) |
| 1 | `StructureStarts` | 4, 5 | |
| 2 | `StructureReferences` | 4, 5 | |
| 3 | `Biomes` | 4, 5 | |
| 4 | `Terrain` | 0, 1, 2, 3 | |
| 5 | `Features` | 0, 1, 2, 3 | |
| 6 | `InitializeLight` | 0, 1, 2, 3 | light sources initialized on load |
| 7 | `Light` | 0, 1, 2, 3 | stored light is used from here on |
| 8 | `Spawn` | 0, 1, 2, 3 | |
| 9 | `Full` | 0, 1, 2, 3 | |

Heightmap ids are in §4.3. Sources: `heightmaps_after` [status.rs:96-108](steel-core/src/chunk/status.rs#L96); light [storage/light.rs:66-68](steel-core/src/chunk_saver/storage/light.rs#L66).

Steel only saves chunks at `StructureStarts` or above ([persistence.rs:23](steel-core/src/chunk/chunk_map/persistence.rs#L23)). Vanilla saves `Empty` chunks that hold a valid structure start ([ChunkMap.java:752](minecraft-src/minecraft/src/net/minecraft/server/level/ChunkMap.java#L752)). Writing such a chunk as `StructureStarts` keeps its structure start.

A chunk below `Full` is a **proto chunk**; generation continues from its status when it is next needed. Fields are used differently by status ([chunk.rs:384-546](steel-core/src/chunk_saver/storage/chunk.rs#L384)):

| Field | Proto (`< Full`) | `Full` |
|---|---|---|
| sections, palettes, structures, postprocessing | used | used |
| heightmaps | only the types for the status; missing ones recomputed | same |
| light | ignored below `Light` | used |
| block entities | stored in the proto chunk | registered with the world |
| entities | stored in the proto chunk | handed to the world's entity manager |
| ticks | duplicates `(pos, type)` removed, first kept | kept exactly as written |
| pois | ignored | used |

### 4.2 Sections, palettes, bit packing

**Sections.** `sections.len()` must equal `height / 16` exactly, ordered bottom to top: section `i` covers `y = min_y + 16·i … min_y + 16·i + 15`. All-air sections must be present too ([chunk.rs:80-86](steel-core/src/chunk_saver/storage/chunk.rs#L80)). Heights: overworld `min_y = -64, height = 384` (24 sections); nether and end `0, 256` (16 sections) (vanilla dimension types: [overworld.json:52-55](minecraft-src/minecraft/resources/data/minecraft/dimension_type/overworld.json#L52), [the_nether.json:36-39](minecraft-src/minecraft/resources/data/minecraft/dimension_type/the_nether.json#L36), [the_end.json:43-46](minecraft-src/minecraft/resources/data/minecraft/dimension_type/the_end.json#L43)).

**Two-level palette.** The chunk has one list of block states and one of biomes. Each section points into them:

```rust
struct PersistentBlockState { name: Identifier, properties: Vec<(&str, &str)> }   // format.rs:294

enum PersistentSection {                                                          // format.rs:397
    Homogeneous   { block_state: u16, biomes: PersistentBiomeData },              // tag 0
    Heterogeneous { palette: Vec<u16>, bits_per_entry: u8, block_data: Box<[u64]>,
                    biomes: PersistentBiomeData },                                // tag 1
}

enum PersistentBiomeData {                                                        // format.rs:420
    Homogeneous   { biome: u16 },                                                 // tag 0
    Heterogeneous { palette: Vec<u16>, bits_per_entry: u8, biome_data: Box<[u64]> }, // tag 1
}
```

- `Homogeneous.block_state` / `.biome` and every `palette` entry are indices into the chunk's `block_states` / `biomes`.
- `block_data` / `biome_data` hold indices into the section's own `palette`.

**Block state rules** ([chunk.rs:714-748](steel-core/src/chunk_saver/storage/chunk.rs#L714)):

- `name` must be a block id Steel's registry knows exactly (no aliases).
- `properties` must list **every** property of that block, each once, with no extras. Order does not matter. Values are the vanilla serialized strings (`"north"`, `"true"`, `"5"`).
- A missing, extra, or unknown property rejects the chunk.

**Biome rules:** each id must be a biome in Steel's registry ([chunk.rs:751-765](steel-core/src/chunk_saver/storage/chunk.rs#L751)).

**Palette rules** ([chunk.rs:24-60](steel-core/src/chunk_saver/storage/chunk.rs#L24), [chunk.rs:88-137](steel-core/src/chunk_saver/storage/chunk.rs#L88)):

- A section (or biome set) with one distinct value **must** be `Homogeneous`. `Heterogeneous` with a palette shorter than 2 is rejected.
- `bits_per_entry` must equal exactly:

  | `palette.len()` | bits |
  |---|---|
  | 2 | 1 |
  | 3–4 | 2 |
  | 5–16 | 4 |
  | 17–256 | 8 |
  | > 256 | 16 |

  ([bit_pack.rs:11](steel-core/src/chunk_saver/bit_pack.rs#L11)). This differs from vanilla, so vanilla data arrays cannot be copied; they must be unpacked and repacked. Vanilla always writes a palette plus indices into it, never global ids. Its bit width is 0 for a single value; for blocks 4 for up to 16 entries, then 5–8; for biomes 1–3; above that `ceil(log2(palette size))` ([PalettedContainer.java:228-241](minecraft-src/minecraft/src/net/minecraft/world/level/chunk/PalettedContainer.java#L228), [Strategy.java:33-60](minecraft-src/minecraft/src/net/minecraft/world/level/chunk/Strategy.java#L33)).
- Word count must be exactly `ceil(entries / (64 / bits))`. For 4096 blocks: 1→64, 2→128, 4→256, 8→512, 16→1024 words. For 64 biome cells: 1→1, 2→2, 4→4, 8→8 words.
- Every palette entry must be a valid chunk-palette index, and every packed value must be a valid section-palette index.
- Not required: deduplicated chunk palettes, or chunk-palette entries being used.

**Index order.**

- Blocks: `index = x + z·16 + y·256` (local 0–15) ([paletted_container.rs:198-202](steel-core/src/chunk/paletted_container.rs#L198)).
- Biomes: `index = qx + qz·4 + qy·16`, with `q = (local_block >> 2) & 3` ([paletted_container.rs:54-55](steel-core/src/chunk/paletted_container.rs#L54), loaded at [chunk.rs:698-705](steel-core/src/chunk_saver/storage/chunk.rs#L698)).

Both match vanilla's order.

**Bit packing.** Entry `i` goes in word `i / (64 / bits)` at bit offset `(i % (64 / bits)) · bits`, least-significant first. Entries never span two words ([bit_pack.rs:35-52](steel-core/src/chunk_saver/bit_pack.rs#L35)).

### 4.3 Heightmaps

`PersistentHeightmap { heightmap_type: u8, data: Vec<u16> }` ([format.rs:307](steel-core/src/chunk_saver/format.rs#L307)).

| Id | Type | Counts blocks that are |
|---|---|---|
| 0 | `WorldSurface` | not air |
| 1 | `MotionBlocking` | blocking motion, or containing fluid |
| 2 | `MotionBlockingNoLeaves` | as 1, and not `#leaves` |
| 3 | `OceanFloor` | blocking motion |
| 4 | `WorldSurfaceWg` | not air |
| 5 | `OceanFloorWg` | blocking motion |

Sources: ids [heightmap.rs:76-98](steel-core/src/chunk/heightmap.rs#L76); predicates [heightmap.rs:148-174](steel-core/src/chunk/heightmap.rs#L148).

- `data` has exactly 256 values, `index = x + z·16` ([heightmap.rs:246-248](steel-core/src/chunk/heightmap.rs#L246)).
- Value = `(highest matching y + 1) − min_y`. `0` means no matching block. Values above `height` reject the chunk; unknown or duplicate types reject it too ([chunk.rs:143-169](steel-core/src/chunk_saver/storage/chunk.rs#L143)).
- Types not listed for the chunk's status (§4.1) are ignored. **Missing types are recomputed from the blocks on load** ([chunk.rs:410-415](steel-core/src/chunk_saver/storage/chunk.rs#L410)), so a converter may write an empty list and skip decoding vanilla's packed `Heightmaps` longs.

### 4.4 Light

```rust
struct PersistentLightData { block: Vec<PersistentLightSection>, sky: Vec<PersistentLightSection> } // format.rs:316
enum PersistentLightSection {                                                                       // format.rs:325
    Uninitialized { section_index: u32 },                 // tag 0: present, all zero
    Initialized   { section_index: u32, data: Vec<u8> },  // tag 1: 2048 bytes
    Internal      { section_index: u32, data: Vec<u8> },  // tag 2: Steel-only, hidden from clients
}
```

- `section_index = section_y − (min_y / 16 − 1)`. Valid range: `0 ≤ section_index < height/16 + 2` (one padding section below and above). Overworld: index 0 = section −5, index 25 = section 20 ([light/section_storage.rs:27-48](steel-core/src/chunk/light/section_storage.rs#L27), [light/mod.rs:15](steel-core/src/chunk/light/mod.rs#L15)).
- Nibble layout, the same as vanilla `DataLayer`: `i = y<<8 | z<<4 | x`; byte `i >> 1`; even `i` = low nibble, odd `i` = high nibble ([light/storage.rs:100-119](steel-core/src/chunk/light/storage.rs#L100)).
- Data must be exactly 2048 bytes. Indices must be in range and unique per layer ([chunk.rs:179-205](steel-core/src/chunk_saver/storage/chunk.rs#L179)).
- A section left out of the list is "missing". On load, missing sky sections below the highest present sky section become zero ([light/storage.rs:302-318](steel-core/src/chunk/light/storage.rs#L302)).
- Light is **ignored for chunks below `Light`**. At `Light` and above it is used as stored and **not recomputed** ([storage/light.rs:62-68](steel-core/src/chunk_saver/storage/light.rs#L62)). A converter should write vanilla `BlockLight`/`SkyLight` arrays as `Initialized` and omit absent sections. It should not emit `Internal`, which vanilla has no equivalent for.

### 4.5 Block entities

`PersistentBlockEntity { x: u8, y: i16, z: u8, entity_type: Option<Identifier>, nbt_data: Vec<u8> }` ([format.rs:442](steel-core/src/chunk_saver/format.rs#L442)).

- `x`, `z` are local 0–15, `y` is absolute. Out-of-range positions reject the chunk ([chunk.rs:209-241](steel-core/src/chunk_saver/storage/chunk.rs#L209)).
- `entity_type` is the block entity type id (vanilla `id`).
- `nbt_data` is a compound body (§0.3) of the vanilla block entity NBT **without** `id`, `x`, `y`, `z`, plus a `components` compound ([block_entity/mod.rs:508-525](steel-core/src/block_entity/mod.rs#L508)). Other keys pass through. An empty vector is allowed.
- `entity_type: None` is vanilla's `DUMMY` pending marker; `nbt_data` is ignored ([chunk.rs:444-447](steel-core/src/chunk_saver/storage/chunk.rs#L444)). Vanilla pending block entities with a real `id` should be written as normal entries.

On load, a problem with one block entity never rejects the chunk ([storage/entities.rs:236-278](steel-core/src/chunk_saver/storage/entities.rs#L236)):

- unknown type → dropped
- type not valid for the block at that position → dropped with a warning
- malformed NBT → dropped with a warning
- an item inside a container that fails to decode → that item is skipped with no warning, and the rest of the container loads ([container/helper.rs:9-22](steel-core/src/inventory/container/helper.rs#L9)). Player files are stricter and reject the whole file (§7.4)
- a known type that Steel has not implemented yet → kept as-is, NBT preserved ([block_entity/registry.rs:83-116](steel-core/src/block_entity/registry.rs#L83))

### 4.6 Entities

Entities are stored **inside the chunk**. Vanilla stores them in two places, and a converter must read both:

- full chunks: separate `entities/*.mca` files ([ServerLevel.java:250](minecraft-src/minecraft/src/net/minecraft/server/level/ServerLevel.java#L250)). Merge each vanilla entity chunk into the matching Steel chunk.
- proto chunks: the chunk NBT's own `entities` list ([SerializableChunkData.java:147](minecraft-src/minecraft/src/net/minecraft/world/level/chunk/storage/SerializableChunkData.java#L147), [SerializableChunkData.java:440-444](minecraft-src/minecraft/src/net/minecraft/world/level/chunk/storage/SerializableChunkData.java#L440)).

`PersistentEntity` ([format.rs:462-511](steel-core/src/chunk_saver/format.rs#L462)), in wire order, with its vanilla key ([Entity.java:2103-2179](minecraft-src/minecraft/src/net/minecraft/world/entity/Entity.java#L2103)):

| # | Field | Type | Vanilla key | Notes |
|---|---|---|---|---|
| 1 | `entity_type` | `Identifier` | `id` | |
| 2 | `uuid` | `[u8; 16]` | `UUID` int[4] | §0.2 |
| 3 | `pos` | `[f64; 3]` | `Pos` | absolute. Non-finite → entity dropped |
| 4 | `motion` | `[f64; 3]` | `Motion` | blocks/tick. Components above 10 become 0 |
| 5 | `rotation` | `[f32; 2]` | `Rotation` | `[yaw, pitch]`, degrees |
| 6 | `fall_distance` | `f64` | `fall_distance` | |
| 7 | `remaining_fire_ticks` | `i32` | `Fire` (short) | sign-extend |
| 8 | `ticks_frozen` | `i32` | `TicksFrozen` | missing → 0 |
| 9 | `is_in_powder_snow` | `bool` | — | write `false` |
| 10 | `was_in_powder_snow` | `bool` | — | write `false` |
| 11 | `has_visual_fire` | `bool` | `HasVisualFire` | |
| 12 | `on_ground` | `bool` | `OnGround` | |
| 13 | `no_gravity` | `bool` | `NoGravity` | |
| 14 | `invulnerable` | `bool` | `Invulnerable` | |
| 15 | `air_supply` | `i32` | `Air` | |
| 16 | `portal_cooldown` | `i32` | `PortalCooldown` | |
| 17 | `custom_name_nbt` | `Vec<u8>` | `CustomName` | empty = no name; else compound body `{CustomName: <component>}` |
| 18 | `custom_name_visible` | `bool` | `CustomNameVisible` | |
| 19 | `silent` | `bool` | `Silent` | |
| 20 | `glowing` | `bool` | `Glowing` | |
| 21 | `tags` | `Vec<String>` | `Tags` | at most 1024 kept |
| 22 | `custom_data_nbt` | `Vec<u8>` | `data` | empty = none; else compound body |
| 23 | `nbt_data` | `Vec<u8>` | everything from `addAdditionalSaveData` | compound body; empty = `{}` |
| 24 | `passengers` | `Vec<PersistentEntity>` | `Passengers` | nested |

Load and save code: [storage/entities.rs:122-198](steel-core/src/chunk_saver/storage/entities.rs#L122), [storage/entities.rs:330-427](steel-core/src/chunk_saver/storage/entities.rs#L330).

**Building `nbt_data`:** take the vanilla entity compound and remove every key in the table above (`id`, `UUID`, `Pos`, `Motion`, `Rotation`, `fall_distance`, `Fire`, `TicksFrozen`, `HasVisualFire`, `OnGround`, `NoGravity`, `Invulnerable`, `Air`, `PortalCooldown`, `CustomName`, `CustomNameVisible`, `Silent`, `Glowing`, `Tags`, `data`, `Passengers`). Write the rest as a compound body. Steel reads only the keys each type implements and ignores the others, so leftovers are harmless but lost on the next save ([storage/entities.rs:393-425](steel-core/src/chunk_saver/storage/entities.rs#L393)). Vanilla's `invulnerable_time` ([Entity.java:2120](minecraft-src/minecraft/src/net/minecraft/world/entity/Entity.java#L2120)) has no Steel field and is dropped.

**Placement rules:**

- Only root entities go in `entities`; riders go in their vehicle's `passengers` ([storage/entities.rs:7-16](steel-core/src/chunk_saver/storage/entities.rs#L7)).
- Store each root entity in the chunk that contains its position. An entity stored elsewhere still loads, with a warning ([storage/entities.rs:367-376](steel-core/src/chunk_saver/storage/entities.rs#L367)), and is tracked under the chunk at its position ([entity/manager/mod.rs:1056](steel-core/src/entity/manager/mod.rs#L1056)). If that chunk is never saved, the entity is lost; Steel only warns at shutdown ([chunk_map/persistence.rs:305-327](steel-core/src/chunk/chunk_map/persistence.rs#L305)).
- UUIDs must be unique across the world. In a full chunk, a tree whose UUID is already in use is dropped with a warning and the chunk is marked dirty ([entity/manager/mod.rs:1040-1052](steel-core/src/entity/manager/mod.rs#L1040), [world/entity_management.rs:159-175](steel-core/src/world/entity_management.rs#L159)). In a proto chunk, two equal UUIDs crash the next save ([storage/mod.rs:596-613](steel-core/src/chunk_saver/storage/mod.rs#L596), [storage/mod.rs:717-727](steel-core/src/chunk_saver/storage/mod.rs#L717)).
- Never write entity types whose `can_serialize` is false: `player`, `leash_knot`, `lightning_bolt`, `fishing_bobber` (`steel-registry/build_assets/entities.json`, [build/entities.rs:30-31](steel-registry/build/entities.rs#L30)).

**What currently survives a load:**

- Only types with a load factory are restored. Today there are 30: `area_effect_cloud`, `bat`, `block_display`, `chest_minecart`, `chicken`, `cow`, `egg`, `endermite`, `ender_pearl`, `end_crystal`, `experience_orb`, `eye_of_ender`, `falling_block`, `firework_rocket`, `fishing_bobber`, `interaction`, `item`, `item_frame`, `leash_knot`, `lingering_potion`, `marker`, `minecart`, `mooshroom`, `pig`, `sheep`, `snowball`, `spawner_minecart`, `splash_potion`, `squid`, `tnt`. The list is generated by [build/entities.rs:24-80](steel-core/build/entities.rs#L24) into `steel-core/src/entity/generated/entities.rs`.
- Any other type, and unknown ids, are **dropped on load** and not written back ([entity/registry.rs:205-223](steel-core/src/entity/registry.rs#L205)). A converter can still write them, but Steel discards them.
- `nbt_data` is read only for the keys each type implements. Examples: item `Health`, `Age`, `PickupDelay`, `Thrower`, `Owner`, `Item` ([item_entity.rs:766-800](steel-core/src/entity/entities/objects/items/item_entity.rs#L766)); mobs `equipment` ([living_entity.rs:214-225](steel-core/src/entity/living_entity.rs#L214)), `drop_chances` ([mob/mod.rs:148](steel-core/src/entity/mob/mod.rs#L148)), `leash` ([leash.rs:444-447](steel-core/src/entity/leash.rs#L444)), and `CanPickUpLoot`, `PersistenceRequired`, `LeftHanded`, `DeathLootTable`, `DeathLootTableSeed`, `NoAI`, `home_pos`, `home_radius` ([mob/mod.rs:1144-1176](steel-core/src/entity/mob/mod.rs#L1144)).
- Living-entity fields `Health`, `HurtTime`, `DeathTime`, `AbsorptionAmount`, `attributes`, and `active_effects` are **not persisted for mobs yet**. They are only written for command NBT ([living_entity.rs:232-262](steel-core/src/entity/living_entity.rs#L232)). `Brain` is not written anywhere.

### 4.7 Scheduled ticks

`PersistentTick { x: u8, y: i16, z: u8, delay: i32, priority: i8, tick_type: Identifier }` ([format.rs:518](steel-core/src/chunk_saver/format.rs#L518)).

- `x`, `z` local, `y` absolute. **A tick outside the chunk rejects the whole chunk**, while vanilla just filters such ticks. Filter them first ([chunk.rs:209-241](steel-core/src/chunk_saver/storage/chunk.rs#L209)).
- `delay` = ticks remaining until it fires (vanilla `t`).
- `priority` = −3 (extremely high) … 3 (extremely low), the same as vanilla `p`. Values outside the range are clamped ([tick_scheduler/mod.rs:62-96](steel-core/src/world/tick_scheduler/mod.rs#L62)).
- `tick_type` = block id for `block_ticks`, fluid id (including `minecraft:flowing_water`, `minecraft:flowing_lava`) for `fluid_ticks`. Unknown ids are dropped ([storage/ticks.rs:51](steel-core/src/chunk_saver/storage/ticks.rs#L51), [storage/ticks.rs:76](steel-core/src/chunk_saver/storage/ticks.rs#L76)).
- **List order is the sub-tick order**: on load each tick gets `trigger = now + delay` in list order ([tick_scheduler/list.rs:279-294](steel-core/src/world/tick_scheduler/list.rs#L279)). Keep vanilla's list order.

### 4.8 Postprocessing

`postprocessing[i]` is the list for section `i`, bottom to top ([full_chunk/mod.rs:1031-1033](steel-core/src/chunk/full_chunk/mod.rs#L1031)). Each `u16` packs local coordinates as `x | y << 4 | z << 8` ([chunk/data.rs:547-552](steel-core/src/chunk/data.rs#L547)). This is the same packing as vanilla `PostProcessing`. Shorter outer lists are padded; more than `height/16` lists reject the chunk ([chunk.rs:243-248](steel-core/src/chunk_saver/storage/chunk.rs#L243)).

### 4.9 Structures

**Starts.** `PersistentStructureStart { structure: Identifier, chunk_x: i32, chunk_z: i32, references: i32, pieces: Vec<PersistentStructurePiece> }` ([format.rs:538](steel-core/src/chunk_saver/format.rs#L538)).

- Stored only in the structure's origin chunk; `chunk_x/z` must be that chunk (vanilla `ChunkX`/`ChunkZ`).
- `references` is vanilla's `references` counter.
- Starts with no pieces are not stored; that is how vanilla `INVALID` is represented ([storage/structures.rs:955](steel-core/src/chunk_saver/storage/structures.rs#L955)).

**Pieces.** `PersistentStructurePiece` ([format.rs:597-614](steel-core/src/chunk_saver/format.rs#L597)), in wire order:

| Field | Type | Vanilla |
|---|---|---|
| `piece_type` | `Identifier` | piece `id` |
| `bounding_box` | 6 × `i32`: `min_x, min_y, min_z, max_x, max_y, max_z` ([format.rs:553](steel-core/src/chunk_saver/format.rs#L553)) | `BB` int[6], same order |
| `gen_depth` | `i32` | `GD` |
| `orientation` | `i8`: −1 none, 0 S, 1 W, 2 N, 3 E | `O` |
| `payload` | enum, see below | family-specific keys |
| `ground_level_delta` | `i32` | `ground_level_delta` |
| `projection` | `i8`: −1 none, 0 rigid, 1 terrain_matching | `projection` |
| `junctions` | `Vec<{source_x: i32, source_ground_y: i32, source_z: i32, delta_y: i32, dest_projection: i8}>` ([format.rs:1119](steel-core/src/chunk_saver/format.rs#L1119)) | `junctions` |

`payload` is `PersistentStructurePiecePayload` ([format.rs:618](steel-core/src/chunk_saver/format.rs#L618)):

- **tag 0, `Jigsaw`** ([format.rs:629](steel-core/src/chunk_saver/format.rs#L629)): `pool_element` ([format.rs:1071](steel-core/src/chunk_saver/format.rs#L1071)), `position: [i32; 3]`, `rotation: i8` (0 none, 1 cw90, 2 cw180, 3 ccw90), `liquid_settings: i8` (0 apply, 1 ignore). Maps from vanilla `PoolElementStructurePiece` keys ([PoolElementStructurePiece.java:71-86](minecraft-src/minecraft/src/net/minecraft/world/level/levelgen/structure/PoolElementStructurePiece.java#L71)).
- **tag 1, `Template`** ([format.rs:642](steel-core/src/chunk_saver/format.rs#L642)): template id, position, rotation, mirror, processors, marker handling, placement adjustment, clip, and post-process. Several of these are not vanilla NBT keys; they are per-family constants Steel sets in each structure's constructor (e.g. [shipwreck.rs:61](steel-worldgen/src/structure/shipwreck.rs#L61), [igloo.rs:44](steel-worldgen/src/structure/igloo.rs#L44), [ocean_ruin.rs:114](steel-worldgen/src/structure/ocean_ruin.rs#L114)). A converter must reproduce those constants per family.
- **tag 2, `Procedural`** ([format.rs:730](steel-core/src/chunk_saver/format.rs#L730)): one variant per procedural family (desert pyramid, jungle temple, swamp hut, mineshaft, nether fortress, ocean monument, stronghold, buried treasure). Its variant `Unimplemented` places nothing but keeps the box, junctions, and projection.

**Families.** Steel has a payload for every vanilla structure family. Piece ids are vanilla's lowercase `StructurePieceType` ids ([StructurePieceType.java](minecraft-src/minecraft/src/net/minecraft/world/level/levelgen/structure/pieces/StructurePieceType.java)):

| Vanilla piece ids | Steel payload | Steel builder |
|---|---|---|
| `jigsaw` (villages, outposts, bastions, ancient cities, trail ruins, trial chambers) | `Jigsaw` | [jigsaw.rs](steel-worldgen/src/structure/jigsaw.rs) |
| `iglu`, `shipwreck`, `orp`, `rupo`, `nefos`, `ecp`, `wmp` | `Template` | [igloo.rs](steel-worldgen/src/structure/igloo.rs), [shipwreck.rs](steel-worldgen/src/structure/shipwreck.rs), [ocean_ruin.rs](steel-worldgen/src/structure/ocean_ruin.rs), [ruined_portal.rs](steel-worldgen/src/structure/ruined_portal.rs), [nether_fossil.rs](steel-worldgen/src/structure/nether_fossil.rs), [end_city.rs](steel-worldgen/src/structure/end_city.rs), [mansion/](steel-worldgen/src/structure/mansion) |
| `btp` | `Procedural(BuriedTreasure)` | [single_piece.rs](steel-worldgen/src/structure/single_piece.rs) |
| `tedp`, `tejp`, `tesh` | `Procedural(DesertPyramid / JungleTemple / SwampHut)` ([format.rs:896-968](steel-core/src/chunk_saver/format.rs#L896)) | [desert_pyramid.rs](steel-worldgen/src/structure/desert_pyramid.rs), [jungle_temple.rs](steel-worldgen/src/structure/jungle_temple.rs), [swamp_hut.rs](steel-worldgen/src/structure/swamp_hut.rs) |
| `mscorridor`, `mscrossing`, `msroom`, `msstairs` | `Procedural(Mineshaft)` ([format.rs:920](steel-core/src/chunk_saver/format.rs#L920)) | [mineshaft.rs](steel-worldgen/src/structure/mineshaft.rs) |
| the 15 fortress ids (`ne*` except `nefos`) | `Procedural(NetherFortress)` ([format.rs:851](steel-core/src/chunk_saver/format.rs#L851)) | [fortress.rs](steel-worldgen/src/structure/fortress.rs) |
| `sh*` (13 ids) | `Procedural(Stronghold)` ([format.rs:766](steel-core/src/chunk_saver/format.rs#L766)) | [stronghold.rs](steel-worldgen/src/structure/stronghold.rs) |
| `omb` | `Procedural(OceanMonument)` ([format.rs:970](steel-core/src/chunk_saver/format.rs#L970)) | [ocean_monument.rs](steel-worldgen/src/structure/ocean_monument.rs) |

Two families don't line up one-to-one:

- **Nether fortress:** vanilla saves the start piece as `nestart`. Steel has no such id and saves the start as a `nebcr` bridge crossing ([fortress.rs:55-57](steel-worldgen/src/structure/fortress.rs#L55)).
- **Ocean monument:** vanilla rebuilds the monument's pieces from the world seed after load ([OceanMonumentStructure.java:54](minecraft-src/minecraft/src/net/minecraft/world/level/levelgen/structure/structures/OceanMonumentStructure.java#L54)). Steel stores the child rooms inside the `omb` payload ([format.rs:977](steel-core/src/chunk_saver/format.rs#L977)), so a converter has to generate them the same way.

This document does not map each payload's fields to vanilla's piece NBT. The payload definitions ([format.rs:618-1130](steel-core/src/chunk_saver/format.rs#L618)) and the builders above are the reference. A converter that cannot fill a payload can write the piece as `Procedural(Unimplemented)` with its real `piece_type` and bounding box, which is what Steel itself uses as the default ([piece.rs:47-58](steel-worldgen/src/structure/piece.rs#L47)). The start then still exists for `/locate` and references, but nothing of it is placed in chunks generated later.

Unknown structure ids are not rejected; their terrain adjustment falls back to none ([storage/structures.rs:1054-1059](steel-core/src/chunk_saver/storage/structures.rs#L1054)).

**References.** `PersistentStructureReference { structure: Identifier, references: Vec<PackedChunkPos> }` ([format.rs:1136](steel-core/src/chunk_saver/format.rs#L1136)). Vanilla `References` long arrays copy over unchanged (§0.2).

**Leaving structures empty is valid** (validation never checks them), with these effects:

- `/locate` won't find structures in converted chunks ([locate.rs:192-199](steel-core/src/command/builtins/locate.rs#L192)).
- Newly generated neighbor chunks won't continue structures that start in converted chunks ([worldgen/stages/structures.rs:32-75](steel-core/src/worldgen/stages/structures.rs#L32)).
- Loaded chunks never regenerate their starts ([worldgen/stages/structures.rs:92-98](steel-core/src/worldgen/stages/structures.rs#L92)).

### 4.10 Points of interest

`PersistentPoi { x: u8, y: i16, z: u8, free_tickets: u32 }` ([format.rs:1148](steel-core/src/chunk_saver/format.rs#L1148)). Used for `Full` chunks only.

- The POI **type is not stored**. On load Steel finds POIs from the block states, then applies `free_tickets` to matching positions ([chunk.rs:464-481](steel-core/src/chunk_saver/storage/chunk.rs#L464)).
- Unmatched positions are ignored. Missing entries start with the type's full ticket count ([poi/poi_instance.rs:15-28](steel-core/src/poi/poi_instance.rs#L15)).
- Vanilla's `poi/*.mca` `free_tickets` map directly ([ChunkMap.java:199](minecraft-src/minecraft/src/net/minecraft/server/level/ChunkMap.java#L199)).

### 4.11 Validation checklist

All of these reject the chunk (§0.4). Source: `validate_persistent_chunk`, [chunk.rs:67-251](steel-core/src/chunk_saver/storage/chunk.rs#L67), plus state resolution at [chunk.rs:714-765](steel-core/src/chunk_saver/storage/chunk.rs#L714). Item 8 fails earlier, while decoding ([region_manager.rs:536-538](steel-core/src/chunk_saver/region_manager.rs#L536), [identifier.rs:203](steel-utils/src/types/identifier.rs#L203)).

1. `sections.len() != height / 16`
2. a palette index outside its palette; a `Heterogeneous` palette with fewer than 2 entries; a wrong `bits_per_entry`; a wrong word count
3. an unknown block, an unknown or missing or extra property, or an unknown biome
4. an unknown or duplicate heightmap type, a heightmap without 256 values, or a value above `height`
5. a light section index out of range or duplicated, or light data that isn't 2048 bytes
6. a block entity, tick, or POI position outside the chunk or the world height
7. more than `height / 16` postprocessing lists
8. an invalid `Identifier` string anywhere, or any malformed wincode

### 4.12 Vanilla chunk keys with no Steel field

Vanilla chunk NBT keys ([SerializableChunkData.java](minecraft-src/minecraft/src/net/minecraft/world/level/chunk/storage/SerializableChunkData.java)) that Steel does not store:

| Vanilla key | What happens |
|---|---|
| `xPos`, `zPos` | implied by the region file and table index (§3.1) |
| `yPos`, section `Y` | implied by the dimension's `min_y` and section order (§4.2) |
| `Status` | goes into the region header byte (§4.1) |
| `DataVersion`, `LastUpdate`, `isLightOn` | dropped |
| `InhabitedTime` | dropped; Steel has no inhabited time |
| `blending_data`, `below_zero_retrogen`, `UpgradeData` | dropped; Steel has no old-world blending or chunk upgrade |

---

## 5. `level.toml`

TOML of `LevelData` ([level_data/mod.rs:71-106](steel-core/src/level_data/mod.rs#L71)). Unknown keys are ignored.

### 5.1 Keys

| Key | Type | Required | Meaning | Vanilla 26.3 source |
|---|---|---|---|---|
| `seed` | i64 | yes | world seed. The file's seed wins over config ([mod.rs:426](steel-core/src/level_data/mod.rs#L426)) | `data/minecraft/world_gen_settings.dat` → `seed` |
| `game_time` | i64 ticks | default world only (§5.2) | domain game time | `level.dat` `Time` |
| `world_clocks` | table | no | §5.3 | `data/minecraft/world_clocks.dat` |
| `spawn` | table | yes | this world's own spawn point: `x`, `y`, `z` (i32 block), `angle` (f32 yaw) ([mod.rs:168](steel-core/src/level_data/mod.rs#L168)) | `level.dat` `spawn`, see below |
| `respawn` | table | no | `dimension` (Steel world key), `x`, `y`, `z` (i32), `yaw`, `pitch` (f32). Read only from the default world ([mod.rs:192](steel-core/src/level_data/mod.rs#L192), [mod.rs:231-271](steel-core/src/level_data/mod.rs#L231)) | `level.dat` `spawn`, see below |
| `weather` | table | yes, every field | `raining`, `thundering` (bool); `rain_time`, `thunder_time`, `clear_weather_time` (i32 ticks) ([mod.rs:275](steel-core/src/level_data/mod.rs#L275)) | `data/minecraft/weather.dat` |
| `world_border` | table | no (all fields default) | §5.4 | `dimensions/<ns>/<path>/data/minecraft/world_border.dat` |
| `difficulty` | integer | no (default 2) | 0 peaceful, 1 easy, 2 normal, 3 hard ([gameplay.rs:107-130](steel-utils/src/types/gameplay.rs#L107)) | `level.dat` `difficulty_settings.difficulty` (string) |
| `difficulty_locked` | bool | no | | `difficulty_settings.locked` |
| `game_rules` | table | **yes** | §5.5 | `data/minecraft/game_rules.dat` |
| `initialized` | bool | **yes** | `false` makes an overworld-type world search for a new spawn. Any other world just sets it to `true` and keeps the file's `spawn` ([world/spawn.rs:25-39](steel-core/src/world/spawn.rs#L25)). Write `true` to keep the vanilla spawn | `level.dat` `initialized` |
| `generation` | table | no | generator settings the world was made with. **Leave it out**: Steel then records the configured settings on first start. A value that differs from config stops the world from loading ([mod.rs:324-343](steel-core/src/level_data/mod.rs#L324)) | no equivalent |

`level.dat` keys with no Steel field: `GameType`, `LevelName`, `allowCommands`, `difficulty_settings.hardcore`, `singleplayer_uuid`, the data-pack and feature-flag lists, `Version`, `DataVersion`, `version`, `version_history`, `ServerBrands`, `WasModded`, `removed_features`, and `LastPlayed` ([PrimaryLevelData.java:129-153](minecraft-src/minecraft/src/net/minecraft/world/level/storage/PrimaryLevelData.java#L129), [PrimaryLevelData.java:181-190](minecraft-src/minecraft/src/net/minecraft/world/level/storage/PrimaryLevelData.java#L181)).

**Spawn.** Vanilla has one world spawn, `level.dat` `spawn` = `{dimension, pos: int[3], yaw, pitch}` ([LevelData.java:33-42](minecraft-src/minecraft/src/net/minecraft/world/level/storage/LevelData.java#L33)). Steel splits it in two:

- `respawn` in the domain default world is where players without a bed respawn. It holds all of vanilla's fields, with `dimension` as a Steel world key ([player_lifecycle.rs:527-553](steel-core/src/server/player_lifecycle.rs#L527)).
- `spawn` is each world's own point. Without `respawn`, Steel uses the default world's `spawn` with pitch 0 ([mod.rs:386-390](steel-core/src/level_data/mod.rs#L386)).

So write vanilla `spawn` into the default world's `respawn`. When its dimension is the default world, also write it into that world's `spawn`, with `angle` = yaw. Vanilla has no spawn for the other dimensions, and Steel doesn't search for one there: `initialized = false` in a nether or end world only flips to `true` and keeps whatever `spawn` the file holds ([world/spawn.rs:36-39](steel-core/src/world/spawn.rs#L36)).

**Generator.** A world's generator is set in `config/worlds.toml` (`generator`, plus an optional `config` table), not in `level.toml` ([config.rs:255-275](steel-core/src/config.rs#L255), [config.rs:536-541](steel-core/src/config.rs#L536)). Choose it from vanilla `world_gen_settings.dat`, which holds the seed, `generate_structures`, `bonus_chest` ([WorldOptions.java:15-18](minecraft-src/minecraft/src/net/minecraft/world/level/levelgen/WorldOptions.java#L15)), and each dimension's generator under `dimensions` ([WorldGenSettings.java:16](minecraft-src/minecraft/src/net/minecraft/world/level/levelgen/WorldGenSettings.java#L16)). If the generator doesn't match, chunks generated later next to converted ones won't line up.

Steel has six generators ([generator/registry.rs:100-140](steel-core/src/worldgen/generator/registry.rs#L100)):

| Vanilla dimension generator | Steel `generator` | Steel `config` |
|---|---|---|
| `noise`, settings `minecraft:overworld`, multi-noise biomes | `minecraft:overworld` | none accepted ([registry.rs:253-261](steel-core/src/worldgen/generator/registry.rs#L253)) |
| `noise`, settings `minecraft:nether` | `minecraft:the_nether` | none accepted |
| `noise`, settings `minecraft:end` | `minecraft:the_end` | none accepted |
| `noise`, settings `minecraft:overworld`, `fixed` biome source | `minecraft:single_biome_surface` | `biome` ([registry.rs:209-212](steel-core/src/worldgen/generator/registry.rs#L209), [registry.rs:357-378](steel-core/src/worldgen/generator/registry.rs#L357)) |
| `flat` | `minecraft:flat` | `layers` (`block`, `height`), `structure_overrides`, `dimension_type`, `features`, `lakes` ([registry.rs:194-205](steel-core/src/worldgen/generator/registry.rs#L194)) |
| — | `steel:empty` | Steel-only |

No Steel equivalent:

- noise settings `minecraft:amplified`, `minecraft:large_biomes`, `minecraft:caves`, `minecraft:floating_islands` ([NoiseGeneratorSettings.java](minecraft-src/minecraft/src/net/minecraft/world/level/levelgen/NoiseGeneratorSettings.java)), the debug world, and custom noise settings or biome sources
- a flat `biome` other than plains: Steel's flat generator has no `biome` setting and uses plains ([flat.rs:21-68](steel-core/src/worldgen/generator/flat.rs#L21))
- flat `features = true` or `lakes = true` (§9)
- `generate_structures = false`: Steel has no setting for it

`bonus_chest` needs no mapping. Vanilla only uses it when it first creates the world ([MinecraftServer.java:430-432](minecraft-src/minecraft/src/net/minecraft/server/MinecraftServer.java#L430)).

### 5.2 `game_time`

- The domain default world **must** have `game_time`. Without it the server refuses to start ([mod.rs:442-450](steel-core/src/level_data/mod.rs#L442)).
- Other worlds must not need it; Steel removes it from them on load ([mod.rs:434-435](steel-core/src/level_data/mod.rs#L434)).

### 5.3 `world_clocks`

A table keyed by clock id. Each value is a `ClockState` ([world/clock.rs:11-29](steel-core/src/world/clock.rs#L11)):

| Field | Type | Default |
|---|---|---|
| `total_ticks` | i64 | 0 |
| `partial_tick` | f32 | 0.0 |
| `rate` | f32 | 1.0 |
| `paused` | bool | false |

```toml
[world_clocks."minecraft:overworld"]
total_ticks = 327297
partial_tick = 0.0
rate = 1.0
paused = false
```

- The registered clocks are `minecraft:overworld` and `minecraft:the_end`. An unknown clock id stops the world from loading ([world/clock.rs:70-93](steel-core/src/world/clock.rs#L70)).
- `rate` must be finite and greater than 0.
- Every world stores every clock, so copy vanilla's server-wide `world_clocks.dat` into each world.
- Vanilla moved `DayTime` into `world_clocks."minecraft:overworld".total_ticks` ([DayTimeToClockFix.java:17-29](minecraft-src/minecraft/src/net/minecraft/util/datafix/fixes/DayTimeToClockFix.java#L17)).

### 5.4 `world_border`

Field names match vanilla `world_border.dat` ([mod.rs:32-67](steel-core/src/level_data/mod.rs#L32)):

| Field | Type | Default |
|---|---|---|
| `center_x`, `center_z` | f64 | 0.0 |
| `damage_per_block` | f64 | 0.2 |
| `safe_zone` | f64 | 5.0 |
| `warning_blocks` | i32 | 5 |
| `warning_time` | i32 ticks ([WorldBorderCommand.java:141](minecraft-src/minecraft/src/net/minecraft/server/commands/WorldBorderCommand.java#L141)) | 300 |
| `size` | f64 | 59999968.0 |
| `lerp_time` | i64 ticks remaining | 0 |
| `lerp_target` | f64 | 0.0 |

Centers must be within ±29999984 and the f64 fields must be finite; otherwise the world fails to load. A `lerp_time` of 0 or less loads as a border that isn't moving ([world/border.rs:313-325](steel-core/src/world/border.rs#L313)).

### 5.5 `game_rules`

- Keys are rule ids **without** the `minecraft:` namespace (`keep_inventory`). Non-vanilla rules keep their namespace ([mod.rs:355-369](steel-core/src/level_data/mod.rs#L355)).
- Values: boolean rules must be TOML booleans, and integer rules TOML integers within the rule's range.
- Unknown names, wrong types, and out-of-range values are ignored, and the rule keeps its default ([game_rules.rs:435-451](steel-registry/src/game_rules.rs#L435)).
- Vanilla `game_rules.dat` stores `"minecraft:<rule>"` with byte or int values: strip the namespace and turn bytes into booleans.
- The rule list (59 rules) is `steel-registry/build_assets/game_rules.json`.

### 5.6 Example (overworld, trimmed)

```toml
seed = 528649801902140233
game_time = 275525
difficulty = 2
difficulty_locked = false
initialized = true

[world_clocks."minecraft:overworld"]
total_ticks = 327297
partial_tick = 0.0
rate = 1.0
paused = false

[spawn]
x = 0
y = 64
z = 0
angle = 0.0

[weather]
raining = false
rain_time = 38878
thundering = false
thunder_time = 55808
clear_weather_time = 0

[game_rules]
keep_inventory = false
respawn_radius = 10
```

---

## 6. `data/*.toml`

TOML files in each world's `data/` directory ([saved_data.rs:20-34](steel-utils/src/saved_data.rs#L20)). A missing file means "empty". Vanilla stores these as gzip NBT `data/<namespace>/<name>.dat` with root `{data, DataVersion}` ([LevelStorageSource.java:168](minecraft-src/minecraft/src/net/minecraft/world/level/storage/LevelStorageSource.java#L168)).

### 6.1 `chunk_tickets.toml` (every world)

```toml
[[tickets]]
type = "minecraft:portal"
chunk_x = 2
chunk_z = 3
level = 127
ticks_left = 280
```

- Schema: [chunk_ticket_storage.rs:21-57](steel-core/src/chunk/chunk_ticket_storage.rs#L21). An empty file is `tickets = []`.
- Only the persistent types are kept: `minecraft:forced` and `minecraft:portal` ([vanilla_ticket_types.rs:40-49](steel-registry/src/vanilla_ticket_types.rs#L40)).
- Invalid entries are skipped.
- **Ticket levels: `steel level = vanilla level + 97`.** Every level on the two scales differs by the same offset:

  | Level | Vanilla ([ChunkLevel.java:10-15](minecraft-src/minecraft/src/net/minecraft/server/level/ChunkLevel.java#L10)) | Steel ([chunk_ticket_manager.rs:9-35](steel-core/src/chunk/chunk_ticket_manager.rs#L9)) |
  |---|---|---|
  | entity ticking | 31 | 128 |
  | block ticking | 32 | 129 |
  | full | 33 | 130 |
  | radius-`r` ticket | 33 − r | 130 − r ([chunk_ticket_manager.rs:49-51](steel-core/src/chunk/chunk_ticket_manager.rs#L49)) |

  Vanilla `forced` tickets are 31 ([ChunkMap.java:126](minecraft-src/minecraft/src/net/minecraft/server/level/ChunkMap.java#L126)), so 128 in Steel. Vanilla `portal` tickets are 33 − 3 = 30 ([Entity.java:3278](minecraft-src/minecraft/src/net/minecraft/world/entity/Entity.java#L3278)), so 127 in Steel, the same as Steel's own portal ticket ([chunk_ticket_storage.rs:116-117](steel-core/src/chunk/chunk_ticket_storage.rs#L116)).
- Vanilla entries are `{chunk_pos: int[2] = [x, z], type, level, ticks_left}` ([TicketStorage.java:36](minecraft-src/minecraft/src/net/minecraft/world/level/TicketStorage.java#L36), [Ticket.java:13-15](minecraft-src/minecraft/src/net/minecraft/server/level/Ticket.java#L13), [ChunkPos.java:20-21](minecraft-src/minecraft/src/net/minecraft/world/level/ChunkPos.java#L20)). They map to `chunk_x`, `chunk_z`, `type`, `level + 97`, and `ticks_left`.

### 6.2 `scoreboard.toml` (domain default world)

```toml
teams = ["blue"]

[objectives.kills]
read_only = false

[scores.Steve.kills]
value = 5
locked = true

[holder_teams]
Steve = "blue"
```

- Unknown keys are an error ([scoreboard.rs:77-129](steel-core/src/scoreboard.rs#L77)).
- `locked` defaults to `true`; vanilla's `Locked` defaults to `false`, so always write it.
- Every score must name an existing objective, and every membership an existing team. A bad file stops the server from starting.
- Only objectives (with `read_only`), scores, teams, and team membership are stored. Display slots, criteria, display names, and team settings are not.

Vanilla source: `data/minecraft/scoreboard.dat` `Objectives`, `PlayerScores`, `Teams` ([ScoreboardSaveData.java:40-52](minecraft-src/minecraft/src/net/minecraft/world/scores/ScoreboardSaveData.java#L40)).

### 6.3 `command_storage.toml` (domain default world)

```toml
[entries]
"minecraft:foo" = [1, 0, 5, 118, 97, 108, 117, 101, 3, 0]   # compound body of {value: 3b}
```

- `entries` maps the storage id (`namespace:path`) to the compound body (§0.3) of its contents, written as a TOML array of bytes ([command/storage.rs:20-72](steel-core/src/command/storage.rs#L20)).
- Each value must be exactly one compound body with no trailing bytes. Empty compounds are dropped. A bad key or value stops the server from starting.

Vanilla source: `data/<namespace>/command_storage.dat` → `contents.<path>` ([CommandStorage.java:63-82](minecraft-src/minecraft/src/net/minecraft/world/level/storage/CommandStorage.java#L63)).

### 6.4 `random_sequences.toml` (domain default world)

```toml
salt = 0
include_world_seed = true
include_sequence_id = true

[sequences."minecraft:blocks/bush"]
source = [6640606061632070130, 2367965058973469832]
```

- `source` is the Xoroshiro128++ state `[seed_lo, seed_hi]` as signed i64 ([random_sequences.rs:24-47](steel-core/src/loot/random_sequences.rs#L24)).
- Maps 1:1 from vanilla `data/minecraft/random_sequences.dat` ([RandomSequences.java:18-29](minecraft-src/minecraft/src/net/minecraft/world/RandomSequences.java#L18)).

---

## 7. Player data

Each domain has its own player files. The vanilla dimensions are domain `minecraft`.

### 7.1 Binary envelope

`<domain>/players/data/<uuid>` and `global/players/data/<uuid>` share this layout ([player_data_storage/mod.rs:547-587](steel-core/src/player/player_data_storage/mod.rs#L547)):

```text
[magic: 4 bytes][version: u16 LE][zstd frame of wincode(payload)]
```

| File | Magic | Version | Payload |
|---|---|---|---|
| `<domain>/players/data/<uuid>` | `STLP` | 10 | `PlayerDataFile` |
| `global/players/data/<uuid>` | `STLG` | 1 | `GlobalPlayerDataFile { data_version: i32 = 1, last_active_domain: String }` ([mod.rs:137](steel-core/src/player/player_data_storage/mod.rs#L137)) |

A wrong magic or version rejects the file. The global file is optional: without it the player joins the default domain ([player_lifecycle.rs:45](steel-core/src/server/player_lifecycle.rs#L45)).

### 7.2 `PlayerDataFile`

[player_data_storage/mod.rs:63-96](steel-core/src/player/player_data_storage/mod.rs#L63), in wire order. The struct is private to `steel-core`, so a tool outside that crate has to reproduce this layout. Vanilla keys are from [Entity.java](minecraft-src/minecraft/src/net/minecraft/world/entity/Entity.java), [LivingEntity.java](minecraft-src/minecraft/src/net/minecraft/world/entity/LivingEntity.java), [Player.java](minecraft-src/minecraft/src/net/minecraft/world/entity/player/Player.java), [ServerPlayer.java](minecraft-src/minecraft/src/net/minecraft/server/level/ServerPlayer.java).

| # | Field | Type | Vanilla key | Notes |
|---|---|---|---|---|
| 1 | `data_version` | i32 | — | must be **7** ([player_data.rs:29](steel-core/src/player/player_data.rs#L29), checked at [mod.rs:378](steel-core/src/player/player_data_storage/mod.rs#L378)) |
| 2 | `pos` | [f64; 3] | `Pos` | |
| 3 | `motion` | [f64; 3] | `Motion` | |
| 4 | `rotation` | [f32; 2] | `Rotation` | [yaw, pitch] |
| 5 | `on_ground` | bool | `OnGround` | |
| 6 | `fall_flying` | bool | `FallFlying` | |
| 7 | `remaining_fire_ticks` | i32 | `Fire` (short) | |
| 8 | `ticks_frozen` | i32 | `TicksFrozen` | |
| 9 | `is_in_powder_snow` | bool | — | `false` |
| 10 | `was_in_powder_snow` | bool | — | `false` |
| 11 | `has_visual_fire` | bool | `HasVisualFire` | |
| 12 | `health` | f32 | `Health` | |
| 13 | `game_mode` | i32 | `playerGameType` | 0 survival, 1 creative, 2 adventure, 3 spectator |
| 14 | `prev_game_mode` | Option<i32> | `previousPlayerGameType` | |
| 15 | `abilities` | `AbilitiesFile` | `abilities` | §7.3 |
| 16 | `inventory` | Vec<`SlotFile`> | `Inventory` + `equipment` | §7.4 |
| 17 | `selected_slot` | i32 | `SelectedItemSlot` | clamped to 0–8 |
| 18 | `world` | String | `Dimension` | Steel world key, e.g. `"minecraft:overworld"` |
| 19 | `food_level` | i32 | `foodLevel` | |
| 20 | `food_saturation_level` | f32 | `foodSaturationLevel` | |
| 21 | `food_exhaustion_level` | f32 | `foodExhaustionLevel` | |
| 22 | `food_tick_timer` | i32 | `foodTickTimer` | |
| 23 | `experience_level` | i32 | `XpLevel` | |
| 24 | `experience_progress` | f32 | `XpP` | |
| 25 | `experience_total` | i32 | `XpTotal` | |
| 26 | `enchantment_seed` | i32 | `XpSeed` | |
| 27 | `score` | i32 | `Score` | |
| 28 | `seen_credits` | bool | `seenCredits` | |
| 29 | `root_vehicle` | Option<`RootVehicleFile`> | `RootVehicle` | §7.3 |
| 30 | `respawn_config` | Option<`RespawnConfigFile`> | `respawn` | §7.3 |
| 31 | `ender_pearls` | Vec<`EnderPearlFile`> | `ender_pearls` | §7.3 |
| 32 | `ender_items` | Vec<`SlotFile`> | `EnderItems` | slots 0–26 |

Vanilla player keys with no Steel field (dropped) include `Air`, `invulnerable_time`, `AbsorptionAmount`, `attributes`, `active_effects`, `recipeBook`, `LastDeathLocation`, `SleepTimer`, `Brain`, and the shoulder entities.

### 7.3 Sub-structures

[player_data_storage/mod.rs:98-134](steel-core/src/player/player_data_storage/mod.rs#L98):

| Struct | Fields (wire order) | Vanilla |
|---|---|---|
| `AbilitiesFile` | `invulnerable, flying, may_fly, instabuild, may_build: bool; flying_speed, walking_speed: f32` | `abilities{invulnerable, flying, mayfly, instabuild, mayBuild, flySpeed, walkSpeed}` |
| `SlotFile` | `slot: i8, item_nbt: Vec<u8>` | §7.4 |
| `RootVehicleFile` | `attach: [u8; 16]` (UUID of the entity the player sits on), `entity: PersistentEntity` (the root vehicle with its passengers, §4.6) | `RootVehicle{Attach, Entity}` |
| `RespawnConfigFile` | `dimension: String` (world key), `pos: [i32; 3]`, `yaw: f32`, `pitch: f32`, `forced: bool` | `respawn{dimension, pos, yaw, pitch, forced}` |
| `EnderPearlFile` | `world: String` (world key), `entity: PersistentEntity` | `ender_pearls[]`, world from `ender_pearl_dimension` |

An unparsable `respawn_config.dimension` rejects the whole player file ([mod.rs:477-485](steel-core/src/player/player_data_storage/mod.rs#L477)).

### 7.4 Inventory slots and items

Steel slot numbers (inventory has 43 slots: [player_inventory/core.rs:57-70](steel-core/src/player/player_inventory/core.rs#L57)):

| Slots | Contents | Vanilla source |
|---|---|---|
| 0–8 | hotbar | `Inventory[Slot 0–8]` |
| 9–35 | main inventory | `Inventory[Slot 9–35]` |
| 36, 37, 38, 39 | feet, legs, chest, head | `equipment.feet/legs/chest/head` |
| 40 | offhand | `equipment.offhand` |
| 41 | body | `equipment.body` |
| 42 | saddle | `equipment.saddle` |

- Write only non-empty slots. Out-of-range slot numbers are ignored ([player_data.rs:486-507](steel-core/src/player/player_data.rs#L486)).
- `item_nbt` is the compound body (§0.3) of `{id: String, count: Int, components?: Compound}`, the vanilla item shape without `Slot` ([item_stack.rs:952-1009](steel-registry/src/item_stack.rs#L952)).
- Components are written `"minecraft:<id>": <value>`, and removed components `"!minecraft:<id>": {}`.
- Loading is strict. **One bad item rejects the whole player file** ([item_stack.rs:1046-1074](steel-registry/src/item_stack.rs#L1046)):
  - an unknown item id
  - a `count` outside 1–99
  - an unknown or non-persistent component
  - a component value that fails to decode
- Never write air.

### 7.5 `stats/<uuid>.toml`

```toml
[stats."minecraft:custom"]
"minecraft:jump" = 21
"minecraft:play_time" = 164622

[stats."minecraft:used"]
"minecraft:fishing_rod" = 57
```

- Outer key = stat type, inner key = stat value id, value = i32 ([stats.rs:9-13](steel-core/src/player/player_data_storage/stats.rs#L9)).
- Same keys as vanilla `players/stats/<uuid>.json` `stats`, without `DataVersion`.
- Unknown stat types and stat ids are skipped with a warning ([stats.rs:39-57](steel-core/src/player/player_data_storage/stats.rs#L39)). An unknown top-level key makes the file fail to parse, and the player loads with empty stats ([file_storage.rs:118-127](steel-core/src/player/player_data_storage/file_storage.rs#L118)).

### 7.6 `advancements/<uuid>.json`

```json
{
  "entries": {
    "minecraft:story/root": {
      "crafting_table": { "secs_since_epoch": 1731800000, "nanos_since_epoch": 0 }
    }
  }
}
```

- Only completed criteria are stored, as serde `SystemTime` ([player/advancement/saving.rs:8-35](steel-core/src/player/advancement/saving.rs#L8)).
- To convert vanilla `players/advancements/<uuid>.json` (`{"<id>": {"criteria": {"<name>": "yyyy-MM-dd HH:mm:ss Z"}, "done": bool}, "DataVersion": N}`):
  1. Parse each timestamp to Unix seconds.
  2. Drop `done` and `DataVersion`.
  3. Wrap the map in `"entries"`.

### 7.7 `global/player_permissions.toml`

```toml
[players."280796c9-bf94-4abd-98bb-f0b89be44d76"]
groups = ["op"]
allow = []
deny = []
metadata = []
```

Schema: [permissions.rs:14-34](steel-core/src/player/player_data_storage/permissions.rs#L14). Steel has no op levels; the built-in group `op` allows everything. How vanilla `ops.json` levels map to groups is a project decision.

---

## 8. Vanilla 26.3 → Steel at a glance

### 8.1 Files

Vanilla saved data is gzip NBT at `data/<ns>/<path>.dat`. Server-wide files are in the save root's `data/` ([MinecraftServer.java:325](minecraft-src/minecraft/src/net/minecraft/server/MinecraftServer.java#L325)). Per-dimension files are in `dimensions/<ns>/<path>/data/`, through the level's own storage ([ServerLevel.java:277](minecraft-src/minecraft/src/net/minecraft/server/level/ServerLevel.java#L277), [ServerLevel.java:1559](minecraft-src/minecraft/src/net/minecraft/server/level/ServerLevel.java#L1559)).

| Vanilla 26.3 | Scope | Steel |
|---|---|---|
| `dimensions/<ns>/<path>/` ([DimensionType.java:114](minecraft-src/minecraft/src/net/minecraft/world/level/dimension/DimensionType.java#L114)) | dimension | `<domain>/worlds/<world>/`. `minecraft:overworld` → domain `minecraft`, world `overworld` |
| `region/*.mca` ([ChunkMap.java:170](minecraft-src/minecraft/src/net/minecraft/server/level/ChunkMap.java#L170)) + `entities/*.mca` + `poi/*.mca` | dimension | one `region/*.srg` holding all three (§3, §4) |
| `level.dat` | server | `level.toml` of each world (§5.1) |
| `data/minecraft/world_gen_settings.dat`, `weather.dat`, `game_rules.dat`, `world_clocks.dat` ([MinecraftServer.java:338-353](minecraft-src/minecraft/src/net/minecraft/server/MinecraftServer.java#L338)) | server | `level.toml` of **each** world (§5) |
| `data/minecraft/scoreboard.dat`, `random_sequences.dat`, `data/<ns>/command_storage.dat` | server | `data/*.toml` in the domain default world (§6) |
| `dimensions/…/data/minecraft/world_border.dat` | dimension | `level.toml` `world_border` (§5.4) |
| `dimensions/…/data/minecraft/chunk_tickets.dat` ([TicketStorage.java:40](minecraft-src/minecraft/src/net/minecraft/world/level/TicketStorage.java#L40)) | dimension | `data/chunk_tickets.toml` (§6.1) |
| `players/data/<uuid>.dat` ([LevelResource.java:4-6](minecraft-src/minecraft/src/net/minecraft/world/level/storage/LevelResource.java#L4)) | server | `<domain>/players/data/<uuid>` (§7) |
| `players/stats/<uuid>.json`, `players/advancements/<uuid>.json` | server | `<domain>/players/stats/<uuid>.toml`, `…/advancements/<uuid>.json` (§7.5, §7.6) |

Steel has no storage for the rest, so a converter drops it:

| Vanilla 26.3 | Scope | Contents |
|---|---|---|
| `data/minecraft/custom_boss_events.dat` ([CustomBossEvents.java:26](minecraft-src/minecraft/src/net/minecraft/server/bossevents/CustomBossEvents.java#L26)) | server | `/bossbar` bars |
| `data/minecraft/scheduled_events.dat` ([TimerQueue.java:24](minecraft-src/minecraft/src/net/minecraft/world/level/timers/TimerQueue.java#L24)) | server | `/schedule` timers |
| `data/minecraft/stopwatches.dat` ([Stopwatches.java:21](minecraft-src/minecraft/src/net/minecraft/world/Stopwatches.java#L21)) | server | stopwatches |
| `data/minecraft/wandering_trader.dat` ([WanderingTraderData.java:16](minecraft-src/minecraft/src/net/minecraft/world/level/saveddata/WanderingTraderData.java#L16)) | server | trader spawn delay and chance |
| `data/minecraft/maps/<id>.dat`, `maps/last_id.dat` ([MapId.java:22](minecraft-src/minecraft/src/net/minecraft/world/level/saveddata/maps/MapId.java#L22), [MapIndex.java:15](minecraft-src/minecraft/src/net/minecraft/world/level/saveddata/maps/MapIndex.java#L15)) | server | map item contents |
| `dimensions/…/data/minecraft/raids.dat` ([Raids.java:31](minecraft-src/minecraft/src/net/minecraft/world/entity/raid/Raids.java#L31)) | dimension | active raids |
| `dimensions/minecraft/the_end/data/minecraft/ender_dragon_fight.dat` ([EnderDragonFight.java:120](minecraft-src/minecraft/src/net/minecraft/world/level/dimension/end/EnderDragonFight.java#L120)) | dimension | dragon fight and gateways. Steel has no dragon fight yet ([end_crystal.rs:60](steel-core/src/behavior/items/end_crystal.rs#L60)) |

### 8.2 Chunk content

| Topic | Vanilla 26.3 | Steel |
|---|---|---|
| Encoding | NBT, deflate by default ([RegionFileVersion.java:45](minecraft-src/minecraft/src/net/minecraft/world/level/chunk/storage/RegionFileVersion.java#L45)) | wincode + zstd |
| Block states | per-section palette of `BlockState.CODEC` entries: a bare block id for a default state, else `{id, properties}`; a missing or invalid property means the default state's value. Strings in a palette that also holds compounds are wrapped as `{"": <id>}` and unwrapped on load ([BlockState.java:10-15](minecraft-src/minecraft/src/net/minecraft/world/level/block/state/BlockState.java#L10), [StateDefinition.java:133-138](minecraft-src/minecraft/src/net/minecraft/world/level/block/state/StateDefinition.java#L133), [ListTag.java:153-178](minecraft-src/minecraft/src/net/minecraft/nbt/ListTag.java#L153)) | chunk palette of name + **all** properties, plus per-section index palettes |
| Bit widths | blocks 4–8, biomes 1–3, then `ceil(log2(palette size))`; always palette + indices | 1, 2, 4, 8, 16 only; repack required |
| Statuses | 10 statuses | same 10, same order |
| Light | `BlockLight`/`SkyLight` per section | same nibble layout, padded section index |
| Entities | `entities/*.mca` | inside the chunk; only 30 types load today |
| POI | `poi/*.mca` with type + tickets | inside the chunk, tickets only |

---

## 9. Known gaps (current Steel behavior)

These are limits in Steel today, not converter choices. Data for items 1–3 is accepted but lost or ignored. Item 4 has no Steel file to go into. Item 5 is rejected as a config error.

1. Only 30 entity types can be loaded; all others are dropped (§4.6).
2. Mob `Health`, `attributes`, `active_effects`, and other living-entity fields are not persisted (§4.6).
3. Scoreboards keep only objectives, scores, teams, and membership (§6.2).
4. Boss bars, `/schedule` timers, stopwatches, maps, raids, the dragon fight, and the wandering trader timer have no Steel storage (§8.1).
5. The flat generator rejects `features = true` and `lakes = true` ([generator/registry.rs:293-299](steel-core/src/worldgen/generator/registry.rs#L293)).

---

## 10. References

Minecraft Wiki pages for the vanilla formats. The wiki may describe versions older than 26.3; when it disagrees with `minecraft-src/`, the source wins.

- [Java Edition level format](https://minecraft.wiki/w/Java_Edition_level_format): save folder layout and `level.dat`
- [Region file format](https://minecraft.wiki/w/Region_file_format) and [Anvil file format](https://minecraft.wiki/w/Anvil_file_format): `.mca` region files
- [Chunk format](https://minecraft.wiki/w/Chunk_format): chunk NBT, sections, palettes, heightmaps
- [Entity format](https://minecraft.wiki/w/Entity_format): entity NBT fields
- [Player.dat format](https://minecraft.wiki/w/Player.dat_format): player data
- [World generation](https://minecraft.wiki/w/World_generation): generation steps (chunk statuses)
