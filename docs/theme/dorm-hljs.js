// Teach mdBook's highlight.js the `dorm` language (the dorm.schema syntax) and
// colour every ```dorm block. Self-contained: it registers the language and
// re-highlights the blocks itself, so it does not depend on when mdBook's own
// highlight pass runs.
(function () {
  function apply() {
    if (!window.hljs) {
      return setTimeout(apply, 50);
    }
    try {
      hljs.registerLanguage("dorm", function (hljs) {
        return {
          name: "DORM Schema",
          keywords: {
            keyword: "datasource generator model enum type",
            literal: "true false null Cascade Restrict SetNull SetDefault NoAction",
            built_in:
              "Int BigInt String Boolean DateTime Decimal Float Json Uuid Bytes",
          },
          contains: [
            hljs.COMMENT("//", "$"),
            { className: "meta", begin: /@@?[A-Za-z_][\w.]*/ },
            {
              className: "built_in",
              begin: /\b(env|now|autoincrement|uuid|cuid|dbgenerated)\b(?=\s*\()/,
            },
            { className: "string", begin: /"/, end: /"/ },
            { className: "number", begin: /\b\d+(\.\d+)?\b/ },
          ],
        };
      });
    } catch (e) {
      /* already registered */
    }
    document.querySelectorAll("code.language-dorm").forEach(function (el) {
      el.removeAttribute("data-highlighted");
      hljs.highlightElement(el);
    });
  }
  if (document.readyState !== "loading") {
    apply();
  } else {
    document.addEventListener("DOMContentLoaded", apply);
  }
})();
