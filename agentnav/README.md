# Rama `agentnav` de este fork de Herdr

Herdr oficial más **cinco temas extra** que aparecen en Settings (`ctrl+b s → theme`) junto a los de fábrica:

| nombre    | qué es                                                                 |
|-----------|------------------------------------------------------------------------|
| `cs16`    | consola de Counter-Strike 1.6 (GoldSrc): paneles verde oliva, texto arena, naranja |
| `matrix`  | negro y fósforo verde                                                  |
| `procomm` | Procomm Plus de DOS: azul CGA, texto blanco, resaltes amarillo y cian   |
| `ambar`   | monitor retro de fósforo ámbar: fondo negro, letras ámbar             |
| `miami`   | atardecer de Vice City: violeta, púrpura, magenta, coral, naranja      |

Herdr no tiene temas de usuario: la lista de Settings sale de `THEME_NAMES` en `src/config/theme.rs` y cada paleta es
una función en `src/app/state.rs`. Por eso el único camino es compilar.

## Qué toca el parche (un solo commit sobre el tag de upstream)

- `src/app/palette_agentnav.rs` — **nuevo**: las cinco paletas (`impl Palette { fn cs16() … }`).
- `src/app/mod.rs` — una línea: `mod palette_agentnav;`.
- `src/config/theme.rs` — cinco nombres en `THEME_NAMES` y cinco brazos en `canonical_theme_name`.
- `src/app/state.rs` — cinco brazos en `Palette::from_name`.
- `src/update.rs` — una constante: `STABLE_UPDATE_MANIFEST_URL` apunta al manifiesto del fork (ver «Updates automáticos»).
- `.github/workflows/agentnav-build.yml` y esta carpeta.

Reglas de los tests de upstream que las paletas cumplen: `sidebar_bg = Color::Reset`, contraste fondo/fila activa ≥ 1.05,
texto/fila activa ≥ 3.0, lo mismo para la fila de selección, y selección ≠ fila activa.

## Cómo se mantiene al día

`agentnav-build.yml` corre cada 6 horas (`17 */6 * * *` UTC), con cada push del parche y a mano desde la pestaña Actions:

1. Busca el último tag `vX.Y.Z` de `herdrdev/herdr`. Si es más nuevo que `agentnav/BASE_TAG`, rebasa esta rama sobre él,
   la deja como **un solo commit** encima del tag y la sube con `--force-with-lease`.
2. Compila `herdr.exe` para Windows x86_64 con el mismo toolchain (`rust-toolchain.toml`), el mismo Zig y los mismos flags
   que el release oficial, corre los tests de temas y publica el release **`agentnav-vX.Y.Z`** con `herdr.exe`,
   `herdr.exe.sha256` y `agentnav-build.json`.
3. Si el rebase choca, aborta, abre un issue en este fork y no publica nada: el release anterior sigue valiendo hasta que
   alguien rehaga el parche (son cinco archivos; el nuevo `palette_agentnav.rs` nunca choca).

Un **push a esta rama** que toque `src/`, `Cargo.*`,
`build.rs` o el workflow recompila y reemplaza los archivos del release actual (misma versión, exe nuevo). El release
siempre queda marcado como *latest*: de eso depende la URL del manifiesto.

## Updates automáticos en la PC

Dos caminos, que se complementan:

- **Dentro de Herdr.** El parche cambia `STABLE_UPDATE_MANIFEST_URL` en `src/update.rs` para que apunte a
  `https://github.com/pablitotester/herdr/releases/latest/download/agentnav-latest.json`. El aviso de versión nueva de
  Herdr y `herdr update` usan ese manifiesto, así que actualizar **no vuelve al oficial**: instala el zip del fork.
  Sólo ve versiones nuevas (`X.Y.Z`); un rebuild del parche con la misma versión no lo nota.
- **Tarea programada `AgentNav Herdr update`.** `install.ps1` copia `agentnav/update.ps1` a `%USERPROFILE%\.herdr\agentnav\`
  y la registra (al iniciar sesión + todos los días a las 13:00; sin admin). Compara el SHA-256 del `herdr.exe` instalado
  con `herdr.exe.sha256` del último release `agentnav-v*`, así que detecta versiones nuevas **y** cambios del parche.
  Si Herdr está abierto no lo cierra: lo deja para el próximo chequeo. Log en `%USERPROFILE%\.herdr\agentnav\update.log`.
  A mano: `... \update.ps1 -Check` (sale con 10 si hay update) o `-Force` (instala aunque Herdr esté abierto).
  `install.ps1 -NoAutoUpdate` no crea la tarea.

Con AgentNav instalado también sirven `agentnav herdr-sync` (baja el último release, verifica el SHA-256, guarda el
exe oficial como `herdr.exe.oficial` y lo reemplaza con el servidor parado) y `agentnav doctor`.

## Volver al oficial

`herdr update` ya **no** sirve para esto (trae el del fork). En su lugar:

    Unregister-ScheduledTask "AgentNav Herdr update" -Confirm:$false
    irm https://herdr.dev/install.ps1 | iex

(o copiar `herdr.exe.oficial` sobre `herdr.exe`, pero el próximo `herdr update` o la tarea vuelven a poner el fork).
