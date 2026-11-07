# Token Benchmark — strut-stack-sql

> **Terbit 2026-11-06.** Angka di bawah ini diukur terhadap binary rilis
> **v2.0.1** dalam dua run independen (run 1 oleh LeBron James,
> 2026-11-05; re-run independen oleh Draymond Green, 2026-11-06) dengan
> payload `tools/list` yang **identik byte-demi-byte**. Ini bukan draf —
> angka run 1 yang semula berstatus draf kini terkonfirmasi dan
> dipublikasikan di dokumen ini.

## 1. Hasil — v2.0.1 (terkonfirmasi dua run)

Diukur terhadap binary **rilis** v2.0.1 (bukan server palsu, bukan build
debug). Run 1: 2026-11-05T18:52:12–13Z (2026-11-06 ~01:52 WIB).
Re-run independen: 2026-11-05T19:47:22–25Z (2026-11-06 ~02:47 WIB),
dieksekusi dari nol oleh tester (bukan penyalinan laporan run 1) +
hitung ulang independen dari nol — hasilnya **identik** dengan run 1
untuk lite dan full.

| Profil | Tool | Raw `tools/list` | Canonical (name+desc+schema) | Token `o200k_base` | Token `cl100k_base` | Cap byte (CI) | Status |
|---|---|---|---|---|---|---|---|
| lite | 3 | **2643 B** | **2629 B** | **625** | **605** | ≤ 3072 B | PASS |
| standard | 13 | 5679 B | 5655 B | 1313 | 1279 | — (tanpa cap CI) | — |
| full | 20 | **10853 B** | **10822 B** | **2549** | **2479** | ≤ 12000 B | PASS |

Versi yang di-pin: server `strut-stack-sql 2.0.1`, commit `805d18f`
(main, PR #7 + PR #8), binary SHA-256
`d76a048560ca87e5e5406c49aaabb55caf74d8581dc7e198a18de3258bdb9cfc`
(asset rilis `strut-stack-sql-linux-x64.tar.gz`, hash resmi terverifikasi),
protocol MCP `2025-06-18`, tiktoken `0.14.0`.
`$schema` stripped: **0** di ketiga profil (server sudah menghilangkan
`$schema` dari `inputSchema`, sesuai harapan).

Bukti determinisme (bukan sekadar total sama): `tools-list-lite.json`
run 2 SHA-256-nya sama persis dengan run 1
(`8679943184d197a3aaef729b815bfd33a2bf33f198b0083114795ffcb984d232`,
`diff` kosong); begitu pula `tools-list-full.json`
(`fd5462192b94a995ea7159d5ebcfdd8d0cb840041e3f5136d8884d8d6628d339`).
Per-tool identik baris demi baris.

Cross-check: angka canonical persis sama dengan prediksi Kawhi
(lite ~2.629 B / full ~10.822 B); angka raw byte persis sama dengan
conformance CI pada main `805d18f` (lite 2643 B, full 10853 B).

### Per tool — lite (3 tool)

| tool | bytes | token `o200k_base` | token `cl100k_base` |
|---|---|---|---|
| analyze_query_performance | 456 | 112 | 104 |
| execute_sql | 1315 | 324 | 316 |
| search_objects | 858 | 189 | 185 |
| **TOTAL** | **2629** | **625** | **605** |

### Per tool — standard (13 tool, baru di run 2)

| tool | bytes | token `o200k_base` | token `cl100k_base` |
|---|---|---|---|
| analyze_query_performance | 456 | 112 | 104 |
| describe_table | 272 | 59 | 57 |
| execute_sql | 1315 | 324 | 316 |
| get_relationship_graph | 549 | 117 | 116 |
| get_schema_context | 392 | 92 | 90 |
| list_foreign_keys | 268 | 62 | 60 |
| list_indexes | 273 | 63 | 62 |
| list_schemas | 175 | 39 | 38 |
| list_sources | 121 | 25 | 24 |
| list_tables | 377 | 91 | 90 |
| schema_overview | 245 | 58 | 57 |
| search_objects | 858 | 189 | 185 |
| suggest_joins | 354 | 82 | 80 |
| **TOTAL** | **5655** | **1313** | **1279** |

### Per tool — full (20 tool)

| tool | bytes | token `o200k_base` | token `cl100k_base` |
|---|---|---|---|
| add_column | 776 | 191 | 186 |
| alter_column | 772 | 188 | 183 |
| analyze_query_performance | 456 | 112 | 104 |
| create_table | 319 | 78 | 75 |
| describe_table | 272 | 59 | 57 |
| drop_column | 865 | 206 | 201 |
| drop_table | 446 | 102 | 100 |
| execute_sql | 1315 | 324 | 316 |
| get_relationship_graph | 549 | 117 | 116 |
| get_schema_context | 392 | 92 | 90 |
| list_foreign_keys | 268 | 62 | 60 |
| list_indexes | 273 | 63 | 62 |
| list_schemas | 175 | 39 | 38 |
| list_sources | 121 | 25 | 24 |
| list_tables | 377 | 91 | 90 |
| schema_mutate | 1535 | 368 | 354 |
| schema_overview | 245 | 58 | 57 |
| search_objects | 858 | 189 | 185 |
| suggest_joins | 354 | 82 | 80 |
| truncate_table | 454 | 103 | 101 |
| **TOTAL** | **10822** | **2549** | **2479** |

Tool termahal: `schema_mutate` (1535 B / 368 token `o200k_base`), lalu
`execute_sql` (1315 B / 324), `drop_column` (865 B / 206),
`search_objects` (858 B / 189). Tool termurah: `list_sources`
(121 B / 25 token).

## 2. Metode

Metode acuan: octomind.run, "Best MCP Servers for Coding Agents in 2026,
Measured by Token Cost" (2026-10-02) — satu hari, satu harness, versi
yang di-pin; ukur apa yang dilihat model, bukan ukuran file.

1. **Spawn binary rilis via stdio**, kirim MCP `initialize`
   (protocol `2025-06-18`) + `tools/list`; simpan JSON mentah.
2. **Yang dihitung: hanya `name` + `description` + `inputSchema` per
   tool** — tiga field inilah yang diubah client menjadi definisi tool
   untuk model. Ikon, wrapper JSON, header, dan `$schema` TIDAK dihitung.
   (Jebakan yang dibuktikan octomind: JSON mentah melebih-lebihkan —
   server GitHub resmi 126 KB = 57.396 token, tapi hanya 11.114 token yang
   mendeskripsikan 46 tool-nya; sisanya ikon PNG base64 untuk UI client.)
3. **Bentuk kanonis**: JSON dengan key terurut, separator compact
   (`(",", ":")`, `ensure_ascii`) — sama dengan `payload_bytes` CI,
   sehingga perbandingan dengan cap byte CI adalah apel-dengan-apel.
4. **Tokenizer: tiktoken `o200k_base` (primer)** — lini OpenAI saat ini —
   **+ `cl100k_base` (sekunder)** untuk komparabilitas dengan angka era
   2025. Selalu sebutkan keduanya; encoding berbeda bisa mengubah hitungan
   20%+ pada teks yang sama. Jangan pernah membandingkan angka
   `cl100k_base` kita dengan angka `o200k_base` orang lain.
5. **Surface assertion (backstop keras)**: `lite==CORE_TOOLS(3)`,
   `full==FULL_TOOLS(20)` — harness gagal keras (exit 2) bila surface
   menyimpang.
6. **Determinisme**: tidak ada timestamp/path absolut di payload yang
   dihitung; urutan tool tetap; versi tiktoken di-pin (`tiktoken==0.14.0`).

### Reproduksi

Harness: `scripts/token-benchmark/measure.py`
(SHA-256 `04135f11ab890930206065c8695cd70ed6bcb7190b3de172ae524569784b7613`,
316 baris).

> **Catatan:** harness kini sudah di-commit ke repo ini (PR #13, commit
> `8f3a44f`, merge `9d4651d`): `scripts/token-benchmark/measure.py`,
> `scripts/token-benchmark/check_budget.py`, dan
> `scripts/token-benchmark/requirements.txt`. File `measure.py` yang
> ter-commit byte-identik dengan yang dipakai kedua run (SHA-256 di atas).
> Perintah di bawah berlaku langsung.

```bash
python3 scripts/token-benchmark/measure.py \
  --binary ~/.local/bin/strut-stack-sql \
  --profile lite --commit 805d18f --out-dir results/v2.0.1

python3 scripts/token-benchmark/measure.py \
  --binary ~/.local/bin/strut-stack-sql \
  --profile full --commit 805d18f --out-dir results/v2.0.1
```

Perhatian: argumen `--profile` di atas adalah **selector di sisi
harness** (memetakan profil ke bentuk spawn CI — lihat §6). Itu BUKAN
flag `--profile` milik binary (flag binary terbukti diabaikan — §6).

### File hasil

Run 1 (`token-benchmark-v2.0.1-run1/`):

| File | SHA-256 |
|---|---|
| `tokens-lite.json` | `45dcd26dd13770906ba55d2e893361e5b64c69573bdcba6b87d67374a9d3fbc4` |
| `tokens-full.json` | `1f20721620ec56cf74b000cf973f749bc74899999276c124d6919e0842c8fd7b` |
| `tools-list-lite.json` | `8679943184d197a3aaef729b815bfd33a2bf33f198b0083114795ffcb984d232` |
| `tools-list-full.json` | `fd5462192b94a995ea7159d5ebcfdd8d0cb840041e3f5136d8884d8d6628d339` |

Run 2 (`token-benchmark-v2.0.1-run2/`):

| File | SHA-256 |
|---|---|
| `tokens-lite.json` | `ef48574aa360bf19762c91e8bdb9af56406af4b711dcb1379959469e27f22dc4` |
| `tokens-full.json` | `475f66a169c38923e053aaf3eef9be04a179078190f1e555f81fc86ea585c1dd` |
| `tokens-standard.json` | `5607b0880430cd7a9818a0b6122083fff990a6147cf019fbd6352fe0089d10f6` |
| `tools-list-lite.json` | `8679943184d197a3aaef729b815bfd33a2bf33f198b0083114795ffcb984d232` (= run 1) |
| `tools-list-full.json` | `fd5462192b94a995ea7159d5ebcfdd8d0cb840041e3f5136d8884d8d6628d339` (= run 1) |
| `tools-list-standard.json` | `4bcc69fdb60db9282cc1db444f7f353f73f53ff5d627b64ed99ec9d55bc800eb` |
| `run2.log` | `1c6eac04520dfd5dab00512edaecfb14c22a588605eba2ffc6b2c82d0fc84945` |

Jumlah per-tool (byte dan kedua encoding) sudah diperiksa ulang:
totalnya sama persis dengan angka headline di §1 — dua kali (harness +
hitung ulang independen pada run 2).

## 3. Apa yang BUKAN diukur angka ini

- **Bukan token sesi.** Ini Fase 1: schema (`tools/list`) saja. Pajak
  schema adalah lantai, bukan total — hasil tool (baris data, schema
  context) mendominasi sesi nyata. Token sesi tugas standar adalah
  **Fase 2 (terpisah, belum dijalankan)**: client MCP scripted,
  fixture SQLite deterministik, skrip tugas tetap dari test prompt
  v2.0.1, satu model tetap, median 3× per server.
- **Bukan tagihan Claude.** Hitungan memakai tokenizer OpenAI
  (tiktoken); Anthropic tidak mempublikasikan tokenizer-nya. Klaim yang
  jujur: "schema token (`o200k_base`)", bukan "token yang akan
  dihabiskan sesi Claude-mu".
- **Bukan perbandingan lintas encoding.** Lihat §2 butir 4.
- **Tidak ada klaim vendor yang dicampur.** Klaim DBHub "1,4k token"
  (Bytebase, tentang produknya sendiri, metode dan tokenizer tidak
  diungkap) adalah **klaim vendor tak terverifikasi — TIDAK dikutip**
  di dokumen ini. Angka kompetitor hanya masuk tabel perbandingan
  setelah diukur ulang dengan harness yang sama, di hari yang sama.
- **Perkiraan bukan pengukuran.** Aturan "~4 karakter/token" hanya
  estimasi internal; estimasi kasar desain (lite ≈660 / full ≈2.700
  token) sudah digantikan angka ukur di §1 (625 / 2549) dan tidak
  dipublikasikan.

## 4. Cap dan budget

- **Cap byte (gate CI, sudah ditegakkan)**: lite ≤ 3072 B, full
  ≤ 12000 B — keduanya PASS pada kedua run (lite sisa 429 B,
  full sisa 1147 B).
- **Budget token (DIADOPSI, keputusan Melo 2026-11-06)**:
  lite ≤ 750 token, full ≤ 3000 token (`o200k_base`). Angka ukur
  (625 / 2549) berada di dalam budget ini (sisa 125 / 451 token).
  Job CI yang menegakkan budget ini adalah pekerjaan follow-up dan
  **belum menjadi gate** — sampai job itu ada, 750/3000 adalah budget
  desain yang diadopsi, bukan gate yang ditegakkan.

## 5. Catatan koreksi README — angka 3.039 B adalah v2.0.0

> **JANGAN dicampur.** Angka **3.039 B** di README lama adalah angka
> **v2.0.0** — bukan v2.0.1.
> Angka v2.0.1 yang berlaku: **lite raw 2643 B, full raw 10853 B**
> (§1). Setiap angka token/byte wajib membawa versi server + tanggal
> pengukuran; angka v2.0.0 tidak berlaku untuk v2.0.1 (deskripsi schema
> berubah di V2 — terbukti oleh CI-fix yang memangkasnya).
>
> Bagian "Token efficiency" di README sudah diperbarui ke angka v2.0.1
> di atas (dengan versi + tanggal) bersamaan dengan terbitnya dokumen
> ini, dan angka 3.039 B tidak lagi muncul sebagai angka v2.0.1.

## 6. Caveat — flag `--profile` diabaikan binary

Ditulis jujur, karena ini mengubah cara angka di atas direproduksi:

- Kedua run memakai **bentuk spawn CI**, bukan flag `--profile`:
  lite = `--ephemeral`; standard = `--full-tools --ephemeral`; full =
  `--url sqlite::memory: --full-tools --allow-ddl`.
- Flag `--profile` milik binary **terbukti DIABAIKAN** oleh binary
  rilis v2.0.1 yang sebenarnya: 8 bentuk spawn diuji (`--profile`
  lite/standard/full/admin, ejaan spasi dan `=`, dengan/tanpa `--url`) —
  semuanya menghasilkan surface lite (3 tool). Yang benar-benar memilih
  surface adalah `--full-tools` (13 tool = standard) + `--allow-ddl`
  (7 tool DDL → 20 tool = full) — persis bentuk CI.
- **Akar penyebab sudah ditemukan** (diverifikasi terhadap main
  `5829bbc8`): `crates/strut-sql-core/src/config.rs:1166` menyelesaikan
  `full_tools` dengan benar (`cli.full_tools || profile...`), tetapi
  dua pemanggilan `build_policy` meneruskan `cli.full_tools` mentah —
  baris **1243** (jalur TOML) dan **1366** (jalur `--url`/ephemeral) —
  sehingga `Policy.full_tools` selalu `false` bila `--full-tools` tidak
  ditulis eksplisit, dan policy menolak 17 tool non-core. Perbaikan
  (2 baris) sedang dikerjakan untuk v2.0.2
  (branch `fix/profile-flag-handoff`).
- Harness sudah diperbaiki: profil dipetakan ke bentuk spawn CI, dan
  surface assertion menjadi backstop (sebelum perbaikan, harness
  memakai `--profile <p> --ephemeral` dan assertion menggagalkannya
  dengan keras, exit 2 — persis sebagaimana dirancang; tanpa perbaikan
  itu, "full" akan mengukur surface lite secara diam-diam).
- **Konsekuensi: angka-angka di §1 menggambarkan surface
  `--full-tools`/`--allow-ddl`, bukan pemanggilan `--profile full`.**
  Bentuk lama `--profile full --allow-ddl` terhadap binary nyata
  memberi surface lite (3 tool), BUKAN 20 tool — jangan pernah dipakai
  untuk mereproduksi angka full.
- **Tidak diukur**: profil `admin` terpisah (harness memetakannya ke
  bentuk full; tidak ada cap CI; tidak ada klaim terpisah).

## 7. Status publikasi

- [x] Run 1 (selesai 2026-11-06 ~01:52 WIB)
- [x] Re-run independen Draymond Green (selesai 2026-11-06 ~02:47 WIB,
      identik byte-demi-byte dengan run 1)
- [x] Terbit ke `docs/TOKEN-BENCHMARK.md` + angka README (PR ini)
- [ ] Angka benchmark ke release notes — masuk release notes **v2.0.2**
      (release notes v2.0.1 sudah terbit dan tidak diubah oleh PR ini)
- [ ] Fase 2 — token sesi tugas standar (terpisah, setelah Fase 1 terbit)
