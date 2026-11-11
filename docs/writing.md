# Write useful documentation

Write for the person deciding whether to use Printable or trying to complete a
task. Start with what they can make, change, check, or retrieve. Name the relevant
capability or integration and link to the detail they need next.

## Review the meaning

- State implemented behavior directly. Give inputs, relevant choices, output
  files, and a way to inspect the result.
- Use plain English and established technical terms. Explain an unfamiliar
  term where the reader needs it, and keep names consistent across guides.
- Use real examples. Keep commands, source, pictures, and reported results
  together so the reader can reproduce and compare them.
- Explain conditions and limits near the decision they affect. Keep general
  setup and recovery material in linked guides instead of repeating it everywhere.
- Check claims against current contracts and behavior. Separate planned work,
  measured software results, backend observations, and physical testing.
- Keep useful explanations and an ordinary, friendly tone. Read a paragraph
  aloud when it sounds awkward; do not reduce it to fragments just to make it short.

## Remove generic prose

Look for sentences that praise the project without explaining anything:
“seamless,” “powerful,” “revolutionary,” and “effortless” usually need a concrete
operation or result in their place. Avoid invented concept labels, stock
introductions and conclusions, fake suspense, rhetorical questions answered by
the next sentence, and repeated “not X, but Y” contrasts.

Revise repeated summaries, forced three-item lists, decorative headings,
emoji clutter, and long sequences of bold labels when they make the information
harder to read. A useful table or numbered procedure can stay. A phrase, list,
or punctuation mark is not proof of who wrote the text; fix the actual reading
problem. AI detection scores and punctuation bans are not review gates.

| Vague wording | Useful wording |
| --- | --- |
| Unlock effortless end-to-end printing | Slice an STL or 3MF with OrcaSlicer, review its toolpaths, then send and start the print through Bambuddy |
| This is not just modeling; it is a creative revolution | Build models with Blender, OpenSCAD, or CadQuery and retain the source and exported files |
| Robust validation ensures every model is print-ready | Read the mesh report for topology, dimensions, overhangs, and the checks that remain unmeasured |
| Simply connect your favorite client | Configure an authenticated streamable HTTP MCP client; use the direct client for downloads if it lacks Printable's file-transfer extension |

## Publish honest pictures

Use actual native screenshots, output renders, toolpath previews, and physical
photographs. Identify which each picture is. A render cannot establish that a
part was printed; an illustration used as model input must be labeled as such.
Never fabricate product UI, success conversations, measurements, or layer lines.

Keep source inputs, relevant settings, and reproduction instructions with each
example. Write useful alt text and captions, and check that dimensions remain
readable on GitHub. Preserve model/image licenses and attribution. Remove
credentials, private addresses, unrelated work, and personal information.

Run `python3 scripts/docgate` after link or heading changes. Check the rendered
page as well: a valid link does not establish that a guide answers its reader's
question. A newcomer trial is separate from an agent successfully running commands.

## References

These guides informed the editorial choices; they are advice for revision,
not a test of authorship:

- [GitHub: About READMEs](https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/customizing-your-repository/about-readmes)
- [Google developer documentation: Voice and tone](https://developers.google.com/style/tone)
- [Microsoft: Style and voice tips](https://learn.microsoft.com/en-us/style-guide/top-10-tips-style-voice)
- [Avoid generated prose tells](https://www.joshka.net/practice/rules/documentation/docs-avoid-generated-prose-tells/)
