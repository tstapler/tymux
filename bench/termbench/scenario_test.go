package main

import (
	"strings"
	"testing"
)

func TestSelectScenarios(t *testing.T) {
	tests := []struct {
		name    string
		csv     string
		want    []string
		wantErr bool
	}{
		{"single", "flood", []string{"flood"}, false},
		{"whitespace trimmed", " flood , echo ", []string{"flood", "echo"}, false},
		{"unknown", "flood,nope", nil, true},
		{"empty", "", nil, true},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got, err := selectScenarios(tt.csv)
			if (err != nil) != tt.wantErr {
				t.Fatalf("err = %v, wantErr %v", err, tt.wantErr)
			}
			if tt.wantErr {
				return
			}
			if len(got) != len(tt.want) {
				t.Fatalf("got %d scenarios, want %d", len(got), len(tt.want))
			}
			for i, s := range got {
				if s.name != tt.want[i] {
					t.Errorf("scenario %d = %q, want %q", i, s.name, tt.want[i])
				}
			}
		})
	}
}

func TestSelectScenariosUnknownNamesTheOffender(t *testing.T) {
	_, err := selectScenarios("bogus")
	if err == nil || !strings.Contains(err.Error(), `"bogus"`) {
		t.Fatalf("err = %v, want it to quote the unknown name", err)
	}
}

func TestDefaultScenariosResolve(t *testing.T) {
	got, err := selectScenarios(strings.Join(scenarioNames(), ","))
	if err != nil {
		t.Fatal(err)
	}
	if len(got) != len(scenarioNames()) {
		t.Fatalf("resolved %d of %d", len(got), len(scenarioNames()))
	}
}
