import { createContentLoader, createMarkdownRenderer } from "vitepress";
import path from "node:path";

const config = globalThis.VITEPRESS_CONFIG;
const md = await createMarkdownRenderer(config.srcDir, config.markdown, config.site.base, config.logger);

export default createContentLoader("./reference/adr/records/*.md", {
    transform(rawData) {
        // For each record, generate the full anchor element, including rendering the title as Markdown, so that
        // we can get the right base URL and formatting all of that.
        rawData.forEach((record) => {
            record.pretty_title = md.render(record.frontmatter.title);
            record.url = path.join(config.site.base, record.url);
        });

        return rawData.sort((a, b) => {
            return +new Date(b.frontmatter.date) - +new Date(a.frontmatter.date);
        });
    },
});
