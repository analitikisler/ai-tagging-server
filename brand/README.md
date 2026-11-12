# Brand

ai tagging server is one of the Analitik İşler ai tools. It has no brand of its own: the family guide and the
design kit decide its names, logos, colours and type. If anything in this repository disagrees with them, they win.

- **Names, logos and voice:** the family guide, `13-ai-brand/BRAND.md` (v1.1) in the Analitik İşler design repository.
- **Colour, type, pattern and controls:** the Çini · Mercan kit, `cini-kit/dist/` in the same repository.

The console uses the kit unchanged. The files it needs are copied into [`src/ui/cini/`](../src/ui/cini/) (tokens,
fonts, base and controls, the Pano pattern, the ai tagging server lockup, the master logo and the favicon), and the
binary embeds them. `src/ui/cini/VERSION` names the kit build they came from.

To update them, rebuild the kit and copy its files over the ones in `src/ui/cini/`. Don't edit them here.
