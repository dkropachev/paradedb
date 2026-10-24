SET paradedb.vector_cluster_max_probe=1.0; SET paradedb.vector_recall_target={{ pg_search_recall_target_unfiltered }}; SELECT _id, title FROM cohere_wiki
WHERE _id @@@ paradedb.all()
ORDER BY emb <=> current_setting('cohere.qvec')::vector(1024)
LIMIT 10;
