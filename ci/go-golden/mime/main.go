package main

import (
	"fmt"
	"io/fs"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"sort"
)

// The asset tree, resolved against the repository root. `cargo xtask go-golden` passes it
// in MW_ROOT; `go run .` from this directory without it falls back to the relative path,
// which works from the repository root.
func assetDir() string {
	if root := os.Getenv("MW_ROOT"); root != "" {
		return filepath.Join(root, "crates/mw-web/assets")
	}
	return "crates/mw-web/assets"
}

func main() {
	dir := os.DirFS(assetDir())
	srv := http.FileServer(http.FS(dir))
	var paths []string
	fs.WalkDir(dir, ".", func(p string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if !d.IsDir() {
			paths = append(paths, "/"+p)
		}
		return nil
	})
	// plus the directory listings DEV-3 removes, and a miss
	paths = append(paths, "/js/", "/css/", "/fonts/", "/images/", "/nonexistent-asset")
	// Sorted so that the table is a function of the tree rather than of the filesystem's
	// walk order -- otherwise a regeneration on another machine is a diff nobody can read.
	sort.Strings(paths)
	fmt.Println("# path\tstatus\tContent-Type")
	for _, p := range paths {
		req := httptest.NewRequest("GET", p, nil)
		rec := httptest.NewRecorder()
		srv.ServeHTTP(rec, req)
		res := rec.Result()
		fmt.Printf("%s\t%d\t%s\n", p, res.StatusCode, res.Header.Get("Content-Type"))
	}
}
