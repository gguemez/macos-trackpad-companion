# Icons

**Option B (`bridge.svg`) is the chosen direction.** Option A is parked
here as an alternative to revisit. Nothing is wired into a build yet.

| File | Use |
| --- | --- |
| `bridge.svg` | **Chosen** app icon — PTP pad, translation chevron, native macOS pointer. Leads with what the program *does*. |
| `bridge-template.svg` | **Chosen** menu-bar template. |
| `pinch-arcs.svg` | Option A app icon — two fingertips closing on each other. Leads with the gesture engine. Parked. |
| `pinch-arcs-template.svg` | Option A menu-bar template. Parked. |

## Conventions

- **App icons** are drawn on a 1024 canvas with the content in an
  824x824 squircle (`rx=185`) inset 100px on every side, matching the
  Big Sur+ icon grid. Export to `.icns` with all the standard sizes.
- **Templates** are pure black on transparency at 18x18. Load them with
  `isTemplate = true` and AppKit handles inversion for dark menu bars,
  selection, and Reduce Transparency — don't ship a separate white copy.
- Templates are **redrawn, not scaled down**. The app-icon geometry has
  detail (bowed shafts, the chevron) that turns to mush at 18pt, so each
  template drops to two chunky shapes.
- The shared accent is `#5AB2FF` on a `#3A4049 → #15181C` graphite
  gradient, so whichever option wins, the pair still reads as a family.
