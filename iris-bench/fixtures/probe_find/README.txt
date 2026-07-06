This fixture's file tree is generated at run time by fixtures::build_find_tree
(a >1000-file tree that trips find's compaction rail). Only this placeholder is
committed so the fixture directory exists for materialize(); the real tree is
built into the temp workspace before the find probe / workload runs.
