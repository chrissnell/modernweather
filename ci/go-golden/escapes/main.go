package main

import (
	"bufio"
	"math"
	"fmt"
	htmltemplate "html/template"
	"os"
	"strings"
)

type ctx struct {
	name   string
	source string
}

func main() {
	var inputs []string
	// Every ASCII codepoint on its own, wrapped so the position is visible.
	for i := 0; i < 128; i++ {
		inputs = append(inputs, fmt.Sprintf("a%cz", rune(i)))
	}
	inputs = append(inputs,
		"", "Suncrest Live Weather", "contract-radar-token",
		"Bob's \"Weather\" & Co", "a+b", "x<script>alert(1)</script>",
		"</script>", "'; alert(1); //", "a\\b", "a/b", "a`b",
		"café", " nbsp", " LS", " PS", "﷐", "￾",
		"tab\there", "nl\nhere", "cr\rhere", "nul\x00here", "\x1b[31m",
		"emoji\U0001F300", "=eq", " space ", "{brace}", "$dollar",
	)

	contexts := []ctx{
		{"text", `<div>{{.}}</div>`},
		{"attr", `<meta content="app-id={{.}}">`},
		{"title", `<title>{{.}}</title>`},
		{"jsstr", `<script id="c" type="application/json">{"t": "{{.}}"}</script>`},
	}

	w := bufio.NewWriter(os.Stdout)
	defer w.Flush()
	fmt.Fprintln(w, "# context\tinput\toutput\tnote")
	for _, c := range contexts {
		t := htmltemplate.Must(htmltemplate.New("t").Parse(c.source))
		for _, in := range inputs {
			var b strings.Builder
			if err := t.Execute(&b, in); err != nil {
				panic(err)
			}
			out := b.String()
			// Strip the fixed prefix and suffix so only the interpolation remains.
			var pre, post string
			switch c.name {
			case "text":
				pre, post = "<div>", "</div>"
			case "attr":
				pre, post = `<meta content="app-id=`, `">`
			case "title":
				pre, post = "<title>", "</title>"
			case "jsstr":
				pre, post = `<script id="c" type="application/json">{"t": "`, `"}</script>`
			}
			if !strings.HasPrefix(out, pre) || !strings.HasSuffix(out, post) {
				panic(fmt.Sprintf("%s: unexpected shape for %q: %q", c.name, in, out))
			}
			mid := out[len(pre) : len(out)-len(post)]
			fmt.Fprintf(w, "%s\t%x\t%x\t%q -> %q\n", c.name, in, mid, in, mid)
		}
	}

	// jsValEscaper, over the numbers and integers the radar-config block interpolates.
	nums := []float64{39.9, -105.5, 0, 1, -0.0, 0.5, 1e21, 1e-7, 1e-6, 123456789012345680000,
		3.141592653589793, -0.000001, 2956007.25, 46.6328125, 180, -180, 1.0}
	t := htmltemplate.Must(htmltemplate.New("v").Parse(`<script id="c" type="application/json">{"n": {{.}}}</script>`))
	for _, n := range nums {
		var b strings.Builder
		if err := t.Execute(&b, n); err != nil {
			panic(err)
		}
		out := b.String()
		pre, post := `<script id="c" type="application/json">{"n": `, `}</script>`
		mid := out[len(pre) : len(out)-len(post)]
		fmt.Fprintf(w, "jsval-f64\t%016x\t%x\t%v -> %q\n", math.Float64bits(n), mid, n, mid)
	}
	ints := []int64{0, 1, 4, -1, 1000000, 9007199254740993}
	ti := htmltemplate.Must(htmltemplate.New("i").Parse(`<script id="c" type="application/json">{"n": {{.}}}</script>`))
	for _, n := range ints {
		var b strings.Builder
		if err := ti.Execute(&b, n); err != nil {
			panic(err)
		}
		out := b.String()
		pre, post := `<script id="c" type="application/json">{"n": `, `}</script>`
		mid := out[len(pre) : len(out)-len(post)]
		fmt.Fprintf(w, "jsval-i64\t%d\t%x\t%d -> %q\n", n, mid, n, mid)
	}
}
