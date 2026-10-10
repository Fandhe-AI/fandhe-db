# crossdb 再計測 集計（Issue #848）

rounds=5 dir=docs/design/bench-data/crossdb-20261009T140039Z-ab

| phase | db/config | n | metric | min-of-N | median | run-to-run 幅 |
| --- | --- | --- | --- | --- | --- | --- |
| agg_count | elasticsearch/exact | 5 | p50_us | 789.42 | 807.96 | 23.5% |
| agg_count | elasticsearch/hnsw | 5 | p50_us | 516.08 | 589.42 | 38.6% |
| agg_count | lancedb/exact | 5 | p50_us | 365.00 | 376.88 | 6.3% |
| agg_count | lancedb/hnsw | 5 | p50_us | 368.38 | 381.83 | 5.8% |
| agg_count | mongodb/exact | 5 | p50_us | 2234.42 | 2292.04 | 9.8% |
| agg_count | mongodb/hnsw | 5 | p50_us | 2297.62 | 2444.79 | 8.8% |
| agg_count | mongodb_plain/exact | 5 | p50_us | 2271.79 | 2432.17 | 8.9% |
| agg_count | mysql/exact | 5 | p50_us | 1972.04 | 2109.13 | 9.0% |
| agg_count | pgvector/exact | 5 | p50_us | 1859.67 | 1879.38 | 5.1% |
| agg_count | pgvector/hnsw | 5 | p50_us | 1736.38 | 1852.71 | 8.8% |
| agg_count | qdrant/exact | 5 | p50_us | 1922.46 | 2002.88 | 24.2% |
| agg_count | qdrant/hnsw | 5 | p50_us | 1627.50 | 1753.37 | 14.4% |
| agg_count | redis/exact | 5 | p50_us | 648.83 | 738.83 | 30.0% |
| agg_count | redis/hnsw | 5 | p50_us | 641.21 | 716.08 | 14.8% |
| agg_count | self/exact | 5 | p50_us | 146.08 | 156.92 | 11.5% |
| agg_count | self/hnsw | 5 | p50_us | 148.08 | 159.21 | 16.0% |
| agg_count | self_nosql/exact | 5 | p50_us | 262.13 | 264.12 | 4.2% |
| agg_count | sqlite_vec/exact | 5 | p50_us | 1036.00 | 1052.21 | 5.0% |
| agg_multi | elasticsearch/exact | 5 | p50_us | 1028.58 | 1071.04 | 27.1% |
| agg_multi | elasticsearch/hnsw | 5 | p50_us | 660.54 | 708.75 | 39.7% |
| agg_multi | lancedb/exact | 5 | p50_us | 5288.58 | 5503.75 | 4.8% |
| agg_multi | lancedb/hnsw | 5 | p50_us | 5296.50 | 5432.00 | 5.7% |
| agg_multi | mongodb/exact | 5 | p50_us | 10030.71 | 10253.67 | 2.5% |
| agg_multi | mongodb/hnsw | 5 | p50_us | 10035.67 | 10076.50 | 3.6% |
| agg_multi | mongodb_plain/exact | 5 | p50_us | 9711.54 | 9766.63 | 1.8% |
| agg_multi | mysql/exact | 5 | p50_us | 2587.17 | 2823.21 | 10.9% |
| agg_multi | pgvector/exact | 5 | p50_us | 2080.33 | 2137.17 | 4.9% |
| agg_multi | pgvector/hnsw | 5 | p50_us | 2083.17 | 2114.58 | 7.5% |
| agg_multi | redis/exact | 5 | p50_us | 6320.04 | 6443.08 | 6.2% |
| agg_multi | redis/hnsw | 5 | p50_us | 6272.83 | 6369.58 | 10.4% |
| agg_multi | self/exact | 5 | p50_us | 564.04 | 572.25 | 7.3% |
| agg_multi | self/hnsw | 5 | p50_us | 559.12 | 576.75 | 5.6% |
| agg_multi | self_nosql/exact | 5 | p50_us | 656.83 | 684.63 | 6.2% |
| agg_multi | sqlite_vec/exact | 5 | p50_us | 1748.12 | 1797.92 | 4.5% |
| agg_multi | （非観測: qdrant/exact, qdrant/hnsw） | 0 | n/a | n/a | n/a | n/a |
| ann_probe | （非観測: elasticsearch/exact, elasticsearch/hnsw, lancedb/exact, lancedb/hnsw, mongodb/exact, mongodb/hnsw, mongodb_plain/exact, mysql/exact, pgvector/exact, pgvector/hnsw, qdrant/exact, qdrant/hnsw, redis/exact, redis/hnsw, self/exact, self/hnsw, self_nosql/exact, sqlite_vec/exact） | 0 | n/a | n/a | n/a | n/a |
| bulk_hybrid_k200 | lancedb/exact | 5 | p50_us | 5235.46 | 5306.00 | 2.7% |
| bulk_hybrid_k200 | lancedb/hnsw | 5 | p50_us | 3683.92 | 3761.00 | 3.9% |
| bulk_hybrid_k200 | mongodb/exact | 5 | p50_us | 11078.46 | 11211.08 | 2.1% |
| bulk_hybrid_k200 | mongodb/hnsw | 5 | p50_us | 10178.58 | 10728.71 | 7.7% |
| bulk_hybrid_k200 | pgvector/exact | 5 | p50_us | 4636.67 | 4809.75 | 6.4% |
| bulk_hybrid_k200 | pgvector/hnsw | 5 | p50_us | 4592.25 | 4628.92 | 2.5% |
| bulk_hybrid_k200 | redis/exact | 5 | p50_us | 1954.58 | 2245.96 | 18.1% |
| bulk_hybrid_k200 | redis/hnsw | 5 | p50_us | 2336.33 | 2420.04 | 7.1% |
| bulk_hybrid_k200 | self/exact | 5 | p50_us | 1618.96 | 1743.13 | 7.9% |
| bulk_hybrid_k200 | self/hnsw | 5 | p50_us | 1529.75 | 1557.83 | 8.9% |
| bulk_hybrid_k200 | self_nosql/exact | 5 | p50_us | 1789.29 | 1891.00 | 8.4% |
| bulk_hybrid_k200 | sqlite_vec/exact | 5 | p50_us | 6438.25 | 6656.83 | 4.1% |
| bulk_hybrid_k200 | （非観測: elasticsearch/exact, elasticsearch/hnsw, mongodb_plain/exact, mysql/exact, qdrant/exact, qdrant/hnsw） | 0 | n/a | n/a | n/a | n/a |
| bulk_knn_k1000 | elasticsearch/exact | 5 | p50_us | 14906.50 | 15268.96 | 5.9% |
| bulk_knn_k1000 | elasticsearch/hnsw | 5 | p50_us | 14415.08 | 14927.50 | 10.8% |
| bulk_knn_k1000 | lancedb/exact | 5 | p50_us | 6158.17 | 6449.63 | 6.5% |
| bulk_knn_k1000 | lancedb/hnsw | 5 | p50_us | 3499.92 | 3624.25 | 8.4% |
| bulk_knn_k1000 | mongodb/exact | 5 | p50_us | 9784.46 | 10488.29 | 12.1% |
| bulk_knn_k1000 | mongodb/hnsw | 5 | p50_us | 11445.62 | 11852.83 | 6.2% |
| bulk_knn_k1000 | pgvector/exact | 5 | p50_us | 4501.00 | 4541.88 | 10.8% |
| bulk_knn_k1000 | pgvector/hnsw | 5 | p50_us | 4669.62 | 4825.83 | 5.9% |
| bulk_knn_k1000 | qdrant/exact | 5 | p50_us | 12339.04 | 12446.29 | 1.2% |
| bulk_knn_k1000 | qdrant/hnsw | 5 | p50_us | 11808.12 | 12024.00 | 3.5% |
| bulk_knn_k1000 | redis/exact | 5 | p50_us | 8451.21 | 8488.42 | 1.0% |
| bulk_knn_k1000 | redis/hnsw | 5 | p50_us | 8799.75 | 8868.71 | 5.7% |
| bulk_knn_k1000 | self/exact | 5 | p50_us | 1436.92 | 1526.38 | 8.4% |
| bulk_knn_k1000 | self/hnsw | 5 | p50_us | 1310.12 | 1431.25 | 11.0% |
| bulk_knn_k1000 | self_nosql/exact | 5 | p50_us | 1752.21 | 1809.21 | 8.6% |
| bulk_knn_k1000 | sqlite_vec/exact | 5 | p50_us | 17737.79 | 18741.71 | 7.3% |
| bulk_knn_k1000 | （非観測: mongodb_plain/exact, mysql/exact） | 0 | n/a | n/a | n/a | n/a |
| bulk_knn_k200 | elasticsearch/exact | 5 | p50_us | 5433.25 | 5505.04 | 6.6% |
| bulk_knn_k200 | elasticsearch/hnsw | 5 | p50_us | 3820.50 | 3879.33 | 5.5% |
| bulk_knn_k200 | lancedb/exact | 5 | p50_us | 4262.17 | 4314.96 | 4.6% |
| bulk_knn_k200 | lancedb/hnsw | 5 | p50_us | 2457.54 | 2478.38 | 6.7% |
| bulk_knn_k200 | mongodb/exact | 5 | p50_us | 3623.33 | 4015.00 | 14.5% |
| bulk_knn_k200 | mongodb/hnsw | 5 | p50_us | 3352.29 | 3729.71 | 16.9% |
| bulk_knn_k200 | pgvector/exact | 5 | p50_us | 3797.67 | 3975.71 | 5.5% |
| bulk_knn_k200 | pgvector/hnsw | 5 | p50_us | 3617.13 | 3760.42 | 6.4% |
| bulk_knn_k200 | qdrant/exact | 5 | p50_us | 3035.25 | 3573.67 | 54.8% |
| bulk_knn_k200 | qdrant/hnsw | 5 | p50_us | 2891.96 | 3174.12 | 90.9% |
| bulk_knn_k200 | redis/exact | 5 | p50_us | 2566.46 | 2679.17 | 42.8% |
| bulk_knn_k200 | redis/hnsw | 5 | p50_us | 2667.50 | 5293.12 | 122.0% |
| bulk_knn_k200 | self/exact | 5 | p50_us | 735.00 | 783.29 | 11.3% |
| bulk_knn_k200 | self/hnsw | 5 | p50_us | 571.42 | 627.46 | 15.8% |
| bulk_knn_k200 | self_nosql/exact | 5 | p50_us | 946.33 | 958.50 | 6.5% |
| bulk_knn_k200 | sqlite_vec/exact | 5 | p50_us | 5304.67 | 5636.79 | 7.3% |
| bulk_knn_k200 | （非観測: mongodb_plain/exact, mysql/exact） | 0 | n/a | n/a | n/a | n/a |
| bulk_knn_where_k200 | elasticsearch/exact | 5 | p50_us | 3654.17 | 3803.25 | 29.9% |
| bulk_knn_where_k200 | elasticsearch/hnsw | 5 | p50_us | 3569.04 | 4063.25 | 23.1% |
| bulk_knn_where_k200 | lancedb/exact | 5 | p50_us | 4508.37 | 4511.58 | 1.6% |
| bulk_knn_where_k200 | lancedb/hnsw | 5 | p50_us | 3317.38 | 3358.58 | 3.2% |
| bulk_knn_where_k200 | mongodb/exact | 5 | p50_us | 3216.71 | 3661.63 | 26.2% |
| bulk_knn_where_k200 | mongodb/hnsw | 5 | p50_us | 3227.54 | 3551.50 | 27.7% |
| bulk_knn_where_k200 | pgvector/exact | 5 | p50_us | 2439.33 | 2562.96 | 5.4% |
| bulk_knn_where_k200 | pgvector/hnsw | 5 | p50_us | 2469.37 | 2549.25 | 5.8% |
| bulk_knn_where_k200 | qdrant/exact | 5 | p50_us | 3161.71 | 3303.00 | 10.0% |
| bulk_knn_where_k200 | qdrant/hnsw | 5 | p50_us | 3085.67 | 3211.50 | 5.5% |
| bulk_knn_where_k200 | redis/exact | 5 | p50_us | 2381.63 | 2420.46 | 8.6% |
| bulk_knn_where_k200 | redis/hnsw | 5 | p50_us | 2511.42 | 2594.75 | 5.8% |
| bulk_knn_where_k200 | self/exact | 5 | p50_us | 407.75 | 424.62 | 8.2% |
| bulk_knn_where_k200 | self/hnsw | 5 | p50_us | 2955.21 | 3148.21 | 7.4% |
| bulk_knn_where_k200 | self_nosql/exact | 5 | p50_us | 545.79 | 553.13 | 11.3% |
| bulk_knn_where_k200 | sqlite_vec/exact | 5 | p50_us | 5959.79 | 6214.00 | 5.0% |
| bulk_knn_where_k200 | （非観測: mongodb_plain/exact, mysql/exact） | 0 | n/a | n/a | n/a | n/a |
| explain | elasticsearch/exact | 5 | p50_us | 2197.00 | 2336.25 | 11.8% |
| explain | elasticsearch/hnsw | 5 | p50_us | 1893.29 | 1906.75 | 16.7% |
| explain | lancedb/exact | 5 | p50_us | 627.46 | 646.50 | 5.6% |
| explain | lancedb/hnsw | 5 | p50_us | 295.58 | 306.21 | 10.1% |
| explain | mongodb/exact | 5 | p50_us | 3504.58 | 3580.92 | 8.2% |
| explain | mongodb/hnsw | 5 | p50_us | 1766.63 | 1937.58 | 18.6% |
| explain | mongodb_plain/exact | 5 | p50_us | 816.21 | 1228.13 | 57.9% |
| explain | mysql/exact | 5 | p50_us | 464.62 | 465.08 | 17.9% |
| explain | pgvector/exact | 5 | p50_us | 266.83 | 270.62 | 27.2% |
| explain | pgvector/hnsw | 5 | p50_us | 270.25 | 308.58 | 38.5% |
| explain | redis/exact | 5 | p50_us | 234.33 | 241.04 | 21.4% |
| explain | redis/hnsw | 5 | p50_us | 234.25 | 239.46 | 5.6% |
| explain | sqlite_vec/exact | 5 | p50_us | 3.29 | 3.33 | 10.1% |
| explain | （非観測: qdrant/exact, qdrant/hnsw, self/exact, self/hnsw, self_nosql/exact） | 0 | n/a | n/a | n/a | n/a |
| group_by_having | elasticsearch/exact | 5 | p50_us | 1049.58 | 1135.04 | 19.4% |
| group_by_having | elasticsearch/hnsw | 5 | p50_us | 621.25 | 717.58 | 33.1% |
| group_by_having | lancedb/exact | 5 | p50_us | 6426.79 | 6546.38 | 2.9% |
| group_by_having | lancedb/hnsw | 5 | p50_us | 6389.21 | 6572.13 | 3.8% |
| group_by_having | mongodb/exact | 5 | p50_us | 8150.29 | 8252.87 | 2.6% |
| group_by_having | mongodb/hnsw | 5 | p50_us | 8147.92 | 8208.96 | 2.7% |
| group_by_having | mongodb_plain/exact | 5 | p50_us | 3990.92 | 4220.04 | 8.0% |
| group_by_having | mysql/exact | 5 | p50_us | 11549.92 | 11666.33 | 3.8% |
| group_by_having | pgvector/exact | 5 | p50_us | 3055.75 | 3120.87 | 4.2% |
| group_by_having | pgvector/hnsw | 5 | p50_us | 3049.12 | 3077.33 | 2.7% |
| group_by_having | redis/exact | 5 | p50_us | 6512.42 | 6651.17 | 3.6% |
| group_by_having | redis/hnsw | 5 | p50_us | 6359.29 | 6497.54 | 4.0% |
| group_by_having | self/exact | 5 | p50_us | 74.58 | 78.75 | 9.7% |
| group_by_having | self/hnsw | 5 | p50_us | 81.21 | 86.88 | 20.3% |
| group_by_having | self_nosql/exact | 5 | p50_us | 182.33 | 191.88 | 7.2% |
| group_by_having | sqlite_vec/exact | 5 | p50_us | 2981.29 | 3062.13 | 4.5% |
| group_by_having | （非観測: qdrant/exact, qdrant/hnsw） | 0 | n/a | n/a | n/a | n/a |
| hnsw_index_warm | （非観測: elasticsearch/exact, elasticsearch/hnsw, lancedb/exact, lancedb/hnsw, mongodb/exact, mongodb/hnsw, mongodb_plain/exact, mysql/exact, pgvector/exact, pgvector/hnsw, qdrant/exact, qdrant/hnsw, redis/exact, redis/hnsw, self/exact, self/hnsw, self_nosql/exact, sqlite_vec/exact） | 0 | n/a | n/a | n/a | n/a |
| hybrid_rrf | lancedb/exact | 5 | p50_us | 3866.37 | 3891.42 | 2.1% |
| hybrid_rrf | lancedb/hnsw | 5 | p50_us | 2297.46 | 2299.96 | 0.3% |
| hybrid_rrf | mongodb/exact | 5 | p50_us | 5577.83 | 5799.29 | 11.3% |
| hybrid_rrf | mongodb/hnsw | 5 | p50_us | 3974.25 | 4084.00 | 9.4% |
| hybrid_rrf | pgvector/exact | 5 | p50_us | 4123.21 | 4295.71 | 9.1% |
| hybrid_rrf | pgvector/hnsw | 5 | p50_us | 4064.50 | 4088.42 | 2.3% |
| hybrid_rrf | redis/exact | 5 | p50_us | 1070.04 | 1384.87 | 37.3% |
| hybrid_rrf | redis/hnsw | 5 | p50_us | 1274.00 | 1570.33 | 32.2% |
| hybrid_rrf | self/exact | 5 | p50_us | 1561.83 | 1617.92 | 5.7% |
| hybrid_rrf | self/hnsw | 5 | p50_us | 1410.79 | 1458.63 | 4.6% |
| hybrid_rrf | self_nosql/exact | 5 | p50_us | 1682.29 | 1739.67 | 7.3% |
| hybrid_rrf | sqlite_vec/exact | 5 | p50_us | 4033.46 | 4100.54 | 2.4% |
| hybrid_rrf | （非観測: elasticsearch/exact, elasticsearch/hnsw, mongodb_plain/exact, mysql/exact, qdrant/exact, qdrant/hnsw） | 0 | n/a | n/a | n/a | n/a |
| index_build | （非観測: elasticsearch/exact, elasticsearch/hnsw, lancedb/exact, lancedb/hnsw, mongodb/exact, mongodb/hnsw, mongodb_plain/exact, mysql/exact, pgvector/exact, pgvector/hnsw, qdrant/exact, qdrant/hnsw, redis/exact, redis/hnsw, self/exact, self/hnsw, self_nosql/exact, sqlite_vec/exact） | 0 | n/a | n/a | n/a | n/a |
| ingest_bulk | elasticsearch/exact | 5 | rows_per_sec | 21859.06 | 22235.36 | 4.2% |
| ingest_bulk | elasticsearch/hnsw | 5 | rows_per_sec | 14037.04 | 14249.40 | 5.0% |
| ingest_bulk | lancedb/exact | 5 | rows_per_sec | 160244.30 | 170296.97 | 35.9% |
| ingest_bulk | lancedb/hnsw | 5 | rows_per_sec | 213193.55 | 222716.07 | 6.5% |
| ingest_bulk | mongodb/exact | 5 | rows_per_sec | 59763.74 | 61091.63 | 3.8% |
| ingest_bulk | mongodb/hnsw | 5 | rows_per_sec | 59312.31 | 60423.81 | 4.3% |
| ingest_bulk | mongodb_plain/exact | 5 | rows_per_sec | 74394.40 | 76170.66 | 8.9% |
| ingest_bulk | mysql/exact | 5 | rows_per_sec | 18347.92 | 19451.93 | 10.0% |
| ingest_bulk | pgvector/exact | 5 | rows_per_sec | 58591.68 | 60460.63 | 5.4% |
| ingest_bulk | pgvector/hnsw | 5 | rows_per_sec | 59090.74 | 60496.10 | 3.7% |
| ingest_bulk | qdrant/exact | 5 | rows_per_sec | 15632.99 | 15876.55 | 2.5% |
| ingest_bulk | qdrant/hnsw | 5 | rows_per_sec | 16228.06 | 16474.16 | 4.9% |
| ingest_bulk | redis/exact | 5 | rows_per_sec | 50468.70 | 51709.87 | 3.6% |
| ingest_bulk | redis/hnsw | 5 | rows_per_sec | 30383.08 | 30888.55 | 1.8% |
| ingest_bulk | sqlite_vec/exact | 5 | rows_per_sec | 92285.25 | 94362.53 | 7.7% |
| ingest_bulk | （非観測: self/exact, self/hnsw, self_nosql/exact） | 0 | n/a | n/a | n/a | n/a |
| ingest_single_stmt | elasticsearch/exact | 5 | rows_per_sec | 638.26 | 757.69 | 27.2% |
| ingest_single_stmt | elasticsearch/hnsw | 5 | rows_per_sec | 695.74 | 856.01 | 51.1% |
| ingest_single_stmt | lancedb/exact | 5 | rows_per_sec | 781.20 | 789.08 | 3.2% |
| ingest_single_stmt | lancedb/hnsw | 5 | rows_per_sec | 716.01 | 772.80 | 11.8% |
| ingest_single_stmt | mongodb/exact | 5 | rows_per_sec | 955.96 | 1093.95 | 22.1% |
| ingest_single_stmt | mongodb/hnsw | 5 | rows_per_sec | 1216.16 | 1297.47 | 23.3% |
| ingest_single_stmt | mongodb_plain/exact | 5 | rows_per_sec | 3165.86 | 3543.43 | 21.1% |
| ingest_single_stmt | mysql/exact | 5 | rows_per_sec | 826.89 | 898.35 | 58.6% |
| ingest_single_stmt | pgvector/exact | 5 | rows_per_sec | 1184.88 | 2188.36 | 113.3% |
| ingest_single_stmt | pgvector/hnsw | 5 | rows_per_sec | 594.06 | 641.96 | 9.5% |
| ingest_single_stmt | qdrant/exact | 5 | rows_per_sec | 1364.71 | 1483.30 | 24.7% |
| ingest_single_stmt | qdrant/hnsw | 5 | rows_per_sec | 564.94 | 864.69 | 85.2% |
| ingest_single_stmt | redis/exact | 5 | rows_per_sec | 3667.42 | 4534.07 | 24.8% |
| ingest_single_stmt | redis/hnsw | 5 | rows_per_sec | 3539.74 | 4052.89 | 16.3% |
| ingest_single_stmt | self/exact | 5 | rows_per_sec | 176.98 | 184.64 | 5.4% |
| ingest_single_stmt | self/hnsw | 5 | rows_per_sec | 180.86 | 188.22 | 7.6% |
| ingest_single_stmt | self_nosql/exact | 5 | rows_per_sec | 149.49 | 172.10 | 16.2% |
| ingest_single_stmt | sqlite_vec/exact | 5 | rows_per_sec | 1772.99 | 1794.77 | 3.7% |
| mode_precision | self/exact | 5 | p50_us | 538.08 | 556.58 | 6.0% |
| mode_precision | self/hnsw | 5 | p50_us | 502.50 | 538.33 | 21.0% |
| mode_precision | self_nosql/exact | 5 | p50_us | 666.12 | 674.75 | 2.0% |
| mode_precision | （非観測: elasticsearch/exact, elasticsearch/hnsw, lancedb/exact, lancedb/hnsw, mongodb/exact, mongodb/hnsw, mongodb_plain/exact, mysql/exact, pgvector/exact, pgvector/hnsw, qdrant/exact, qdrant/hnsw, redis/exact, redis/hnsw, sqlite_vec/exact） | 0 | n/a | n/a | n/a | n/a |
| mode_recall | self/exact | 5 | p50_us | 535.96 | 558.83 | 9.0% |
| mode_recall | self/hnsw | 5 | p50_us | 426.62 | 464.58 | 10.7% |
| mode_recall | self_nosql/exact | 5 | p50_us | 673.21 | 684.25 | 4.2% |
| mode_recall | （非観測: elasticsearch/exact, elasticsearch/hnsw, lancedb/exact, lancedb/hnsw, mongodb/exact, mongodb/hnsw, mongodb_plain/exact, mysql/exact, pgvector/exact, pgvector/hnsw, qdrant/exact, qdrant/hnsw, redis/exact, redis/hnsw, sqlite_vec/exact） | 0 | n/a | n/a | n/a | n/a |
| point_where | elasticsearch/exact | 5 | p50_us | 1682.37 | 1806.42 | 18.1% |
| point_where | elasticsearch/hnsw | 5 | p50_us | 1454.08 | 1734.46 | 25.6% |
| point_where | lancedb/exact | 5 | p50_us | 3453.29 | 3491.17 | 2.5% |
| point_where | lancedb/hnsw | 5 | p50_us | 2121.46 | 2261.08 | 10.1% |
| point_where | mongodb/exact | 5 | p50_us | 1890.17 | 1903.25 | 5.7% |
| point_where | mongodb/hnsw | 5 | p50_us | 2557.21 | 2592.17 | 9.9% |
| point_where | pgvector/exact | 5 | p50_us | 2126.04 | 2144.33 | 4.5% |
| point_where | pgvector/hnsw | 5 | p50_us | 2128.54 | 2166.00 | 6.4% |
| point_where | qdrant/exact | 5 | p50_us | 619.71 | 643.08 | 10.8% |
| point_where | qdrant/hnsw | 5 | p50_us | 640.96 | 675.04 | 110.5% |
| point_where | redis/exact | 5 | p50_us | 822.83 | 1030.96 | 37.2% |
| point_where | redis/hnsw | 5 | p50_us | 918.71 | 948.46 | 31.4% |
| point_where | self/exact | 5 | p50_us | 236.92 | 244.37 | 5.5% |
| point_where | self/hnsw | 5 | p50_us | 2207.88 | 2379.21 | 9.6% |
| point_where | self_nosql/exact | 5 | p50_us | 354.54 | 367.00 | 11.0% |
| point_where | sqlite_vec/exact | 5 | p50_us | 2076.13 | 2173.50 | 14.2% |
| point_where | （非観測: mongodb_plain/exact, mysql/exact） | 0 | n/a | n/a | n/a | n/a |
| recall_at_10 | （非観測: elasticsearch/exact, elasticsearch/hnsw, lancedb/exact, lancedb/hnsw, mongodb/exact, mongodb/hnsw, mongodb_plain/exact, mysql/exact, pgvector/exact, pgvector/hnsw, qdrant/exact, qdrant/hnsw, redis/exact, redis/hnsw, self/exact, self/hnsw, self_nosql/exact, sqlite_vec/exact） | 0 | n/a | n/a | n/a | n/a |
| rls_isolation | elasticsearch/exact | 5 | p50_us | 665.29 | 693.50 | 23.8% |
| rls_isolation | elasticsearch/hnsw | 5 | p50_us | 447.38 | 469.58 | 22.4% |
| rls_isolation | lancedb/exact | 5 | p50_us | 381.50 | 383.96 | 1.9% |
| rls_isolation | lancedb/hnsw | 5 | p50_us | 372.54 | 388.83 | 4.7% |
| rls_isolation | mongodb/exact | 5 | p50_us | 2355.13 | 2418.58 | 6.7% |
| rls_isolation | mongodb/hnsw | 5 | p50_us | 2235.46 | 2243.25 | 7.7% |
| rls_isolation | mongodb_plain/exact | 5 | p50_us | 2299.04 | 2436.96 | 10.5% |
| rls_isolation | mysql/exact | 5 | p50_us | 1973.75 | 2027.00 | 9.0% |
| rls_isolation | pgvector/exact | 5 | p50_us | 1848.00 | 1905.50 | 6.4% |
| rls_isolation | pgvector/hnsw | 5 | p50_us | 1773.83 | 1830.38 | 5.2% |
| rls_isolation | qdrant/exact | 5 | p50_us | 1901.04 | 2080.54 | 21.5% |
| rls_isolation | qdrant/hnsw | 5 | p50_us | 1538.79 | 1672.54 | 36.2% |
| rls_isolation | redis/exact | 5 | p50_us | 669.42 | 680.08 | 7.5% |
| rls_isolation | redis/hnsw | 5 | p50_us | 655.37 | 662.58 | 7.3% |
| rls_isolation | self/exact | 5 | p50_us | 166.08 | 180.58 | 12.3% |
| rls_isolation | self/hnsw | 5 | p50_us | 162.92 | 171.63 | 11.0% |
| rls_isolation | self_nosql/exact | 5 | p50_us | 266.46 | 269.96 | 4.5% |
| rls_isolation | sqlite_vec/exact | 5 | p50_us | 1049.21 | 1068.63 | 3.8% |
| scan_where_nosort_k500 | elasticsearch/exact | 5 | p50_us | 6986.92 | 7333.58 | 11.9% |
| scan_where_nosort_k500 | elasticsearch/hnsw | 5 | p50_us | 5832.79 | 7191.00 | 27.2% |
| scan_where_nosort_k500 | lancedb/exact | 5 | p50_us | 1357.67 | 1386.37 | 3.0% |
| scan_where_nosort_k500 | lancedb/hnsw | 5 | p50_us | 1351.88 | 1384.67 | 3.8% |
| scan_where_nosort_k500 | mongodb/exact | 5 | p50_us | 1173.21 | 1749.25 | 91.6% |
| scan_where_nosort_k500 | mongodb/hnsw | 5 | p50_us | 1192.87 | 1258.04 | 94.0% |
| scan_where_nosort_k500 | mongodb_plain/exact | 5 | p50_us | 2299.63 | 2367.42 | 12.5% |
| scan_where_nosort_k500 | mysql/exact | 5 | p50_us | 1110.75 | 1864.17 | 116.8% |
| scan_where_nosort_k500 | pgvector/exact | 5 | p50_us | 1064.00 | 1188.54 | 24.8% |
| scan_where_nosort_k500 | pgvector/hnsw | 5 | p50_us | 1076.83 | 1287.25 | 20.5% |
| scan_where_nosort_k500 | qdrant/exact | 5 | p50_us | 4452.75 | 4948.25 | 28.0% |
| scan_where_nosort_k500 | qdrant/hnsw | 5 | p50_us | 4566.83 | 4639.46 | 19.5% |
| scan_where_nosort_k500 | redis/exact | 5 | p50_us | 4223.67 | 4668.08 | 19.1% |
| scan_where_nosort_k500 | redis/hnsw | 5 | p50_us | 4223.62 | 4415.71 | 14.9% |
| scan_where_nosort_k500 | self/exact | 5 | p50_us | 447.50 | 485.50 | 10.3% |
| scan_where_nosort_k500 | self/hnsw | 5 | p50_us | 461.25 | 495.67 | 9.5% |
| scan_where_nosort_k500 | self_nosql/exact | 5 | p50_us | 642.46 | 675.08 | 7.2% |
| scan_where_nosort_k500 | sqlite_vec/exact | 5 | p50_us | 198.21 | 202.33 | 3.8% |
| udf_call | self/exact | 5 | p50_us | 543.96 | 580.79 | 12.6% |
| udf_call | self/hnsw | 5 | p50_us | 419.92 | 435.29 | 5.8% |
| udf_call | （非観測: elasticsearch/exact, elasticsearch/hnsw, lancedb/exact, lancedb/hnsw, mongodb/exact, mongodb/hnsw, mongodb_plain/exact, mysql/exact, pgvector/exact, pgvector/hnsw, qdrant/exact, qdrant/hnsw, redis/exact, redis/hnsw, self_nosql/exact, sqlite_vec/exact） | 0 | n/a | n/a | n/a | n/a |
| vector_knn | elasticsearch/exact | 5 | p50_us | 3736.08 | 3923.29 | 8.1% |
| vector_knn | elasticsearch/hnsw | 5 | p50_us | 1735.87 | 1865.46 | 8.0% |
| vector_knn | lancedb/exact | 5 | p50_us | 3158.58 | 3159.58 | 2.3% |
| vector_knn | lancedb/hnsw | 5 | p50_us | 1561.62 | 1568.38 | 9.2% |
| vector_knn | mongodb/exact | 5 | p50_us | 2677.13 | 2779.38 | 13.7% |
| vector_knn | mongodb/hnsw | 5 | p50_us | 1492.50 | 1695.96 | 27.0% |
| vector_knn | pgvector/exact | 5 | p50_us | 3354.71 | 3395.54 | 4.2% |
| vector_knn | pgvector/hnsw | 5 | p50_us | 3193.25 | 3218.71 | 5.7% |
| vector_knn | qdrant/exact | 5 | p50_us | 739.88 | 757.04 | 7.5% |
| vector_knn | qdrant/hnsw | 5 | p50_us | 700.25 | 1198.96 | 156.9% |
| vector_knn | redis/exact | 5 | p50_us | 991.71 | 993.50 | 8.0% |
| vector_knn | redis/hnsw | 5 | p50_us | 1087.67 | 1147.33 | 29.7% |
| vector_knn | self/exact | 5 | p50_us | 537.04 | 544.29 | 10.2% |
| vector_knn | self/hnsw | 5 | p50_us | 431.33 | 455.25 | 8.5% |
| vector_knn | self_nosql/exact | 5 | p50_us | 667.42 | 688.00 | 6.0% |
| vector_knn | sqlite_vec/exact | 5 | p50_us | 2500.00 | 2624.67 | 6.3% |
| vector_knn | （非観測: mongodb_plain/exact, mysql/exact） | 0 | n/a | n/a | n/a | n/a |
| vector_knn_pipeline_bruteforce | mongodb_plain/exact | 5 | p50_us | 477649.96 | 478900.83 | 15.9% |
| vector_knn_pipeline_bruteforce | （非観測: elasticsearch/exact, elasticsearch/hnsw, lancedb/exact, lancedb/hnsw, mongodb/exact, mongodb/hnsw, mysql/exact, pgvector/exact, pgvector/hnsw, qdrant/exact, qdrant/hnsw, redis/exact, redis/hnsw, self/exact, self/hnsw, self_nosql/exact, sqlite_vec/exact） | 0 | n/a | n/a | n/a | n/a |
| vector_knn_where | elasticsearch/exact | 5 | p50_us | 1682.37 | 1806.42 | 18.1% |
| vector_knn_where | elasticsearch/hnsw | 5 | p50_us | 1454.08 | 1734.46 | 25.6% |
| vector_knn_where | lancedb/exact | 5 | p50_us | 3453.29 | 3491.17 | 2.5% |
| vector_knn_where | lancedb/hnsw | 5 | p50_us | 2121.46 | 2261.08 | 10.1% |
| vector_knn_where | mongodb/exact | 5 | p50_us | 1890.17 | 1903.25 | 5.7% |
| vector_knn_where | mongodb/hnsw | 5 | p50_us | 2557.21 | 2592.17 | 9.9% |
| vector_knn_where | pgvector/exact | 5 | p50_us | 2126.04 | 2144.33 | 4.5% |
| vector_knn_where | pgvector/hnsw | 5 | p50_us | 2128.54 | 2166.00 | 6.4% |
| vector_knn_where | qdrant/exact | 5 | p50_us | 619.71 | 643.08 | 10.8% |
| vector_knn_where | qdrant/hnsw | 5 | p50_us | 640.96 | 675.04 | 110.5% |
| vector_knn_where | redis/exact | 5 | p50_us | 822.83 | 1030.96 | 37.2% |
| vector_knn_where | redis/hnsw | 5 | p50_us | 918.71 | 948.46 | 31.4% |
| vector_knn_where | self/exact | 5 | p50_us | 236.92 | 244.37 | 5.5% |
| vector_knn_where | self/hnsw | 5 | p50_us | 2207.88 | 2379.21 | 9.6% |
| vector_knn_where | self_nosql/exact | 5 | p50_us | 354.54 | 367.00 | 11.0% |
| vector_knn_where | sqlite_vec/exact | 5 | p50_us | 2076.13 | 2173.50 | 14.2% |
| vector_knn_where | （非観測: mongodb_plain/exact, mysql/exact） | 0 | n/a | n/a | n/a | n/a |
| where_compound_count | elasticsearch/exact | 5 | p50_us | 889.92 | 941.33 | 13.4% |
| where_compound_count | elasticsearch/hnsw | 5 | p50_us | 522.29 | 668.62 | 45.1% |
| where_compound_count | lancedb/exact | 5 | p50_us | 605.75 | 625.33 | 6.5% |
| where_compound_count | lancedb/hnsw | 5 | p50_us | 600.63 | 628.46 | 11.4% |
| where_compound_count | mongodb/exact | 5 | p50_us | 3307.00 | 3502.79 | 8.3% |
| where_compound_count | mongodb/hnsw | 5 | p50_us | 3336.38 | 3479.71 | 7.0% |
| where_compound_count | mongodb_plain/exact | 5 | p50_us | 2708.71 | 2716.63 | 0.8% |
| where_compound_count | mysql/exact | 5 | p50_us | 3585.50 | 3649.42 | 3.8% |
| where_compound_count | pgvector/exact | 5 | p50_us | 1534.62 | 1578.42 | 10.3% |
| where_compound_count | pgvector/hnsw | 5 | p50_us | 1473.54 | 1616.54 | 19.8% |
| where_compound_count | qdrant/exact | 5 | p50_us | 7277.96 | 7607.00 | 8.0% |
| where_compound_count | qdrant/hnsw | 5 | p50_us | 7243.75 | 7386.08 | 6.2% |
| where_compound_count | redis/exact | 5 | p50_us | 1135.12 | 1244.67 | 12.3% |
| where_compound_count | redis/hnsw | 5 | p50_us | 1041.21 | 1047.50 | 3.4% |
| where_compound_count | self/exact | 5 | p50_us | 98.08 | 101.33 | 5.3% |
| where_compound_count | self/hnsw | 5 | p50_us | 113.37 | 116.67 | 7.3% |
| where_compound_count | sqlite_vec/exact | 5 | p50_us | 1366.87 | 1387.88 | 4.6% |
| where_compound_count | （非観測: self_nosql/exact） | 0 | n/a | n/a | n/a | n/a |

## self との比較（固定 ±5% 帯かつ両 arm の run-to-run 幅を超える場合のみ win/loss。それ以外は僅差）

| phase | 対照 db/config | self min-of-N | 対照 min-of-N | 比(対照/self) | 判定 |
| --- | --- | --- | --- | --- | --- |
| agg_count | elasticsearch/exact | 146.08 | 789.42 | 5.404 | self win |
| agg_count | elasticsearch/hnsw | 146.08 | 516.08 | 3.533 | self win |
| agg_count | lancedb/exact | 146.08 | 365.00 | 2.499 | self win |
| agg_count | lancedb/hnsw | 146.08 | 368.38 | 2.522 | self win |
| agg_count | mongodb/exact | 146.08 | 2234.42 | 15.296 | self win |
| agg_count | mongodb/hnsw | 146.08 | 2297.62 | 15.728 | self win |
| agg_count | mongodb_plain/exact | 146.08 | 2271.79 | 15.551 | self win |
| agg_count | mysql/exact | 146.08 | 1972.04 | 13.499 | self win |
| agg_count | pgvector/exact | 146.08 | 1859.67 | 12.730 | self win |
| agg_count | pgvector/hnsw | 146.08 | 1736.38 | 11.886 | self win |
| agg_count | qdrant/exact | 146.08 | 1922.46 | 13.160 | self win |
| agg_count | qdrant/hnsw | 146.08 | 1627.50 | 11.141 | self win |
| agg_count | redis/exact | 146.08 | 648.83 | 4.442 | self win |
| agg_count | redis/hnsw | 146.08 | 641.21 | 4.389 | self win |
| agg_count | self/hnsw | 146.08 | 148.08 | 1.014 | 僅差 |
| agg_count | self_nosql/exact | 146.08 | 262.13 | 1.794 | self win |
| agg_count | sqlite_vec/exact | 146.08 | 1036.00 | 7.092 | self win |
| agg_multi | elasticsearch/exact | 564.04 | 1028.58 | 1.824 | self win |
| agg_multi | elasticsearch/hnsw | 564.04 | 660.54 | 1.171 | 僅差 |
| agg_multi | lancedb/exact | 564.04 | 5288.58 | 9.376 | self win |
| agg_multi | lancedb/hnsw | 564.04 | 5296.50 | 9.390 | self win |
| agg_multi | mongodb/exact | 564.04 | 10030.71 | 17.784 | self win |
| agg_multi | mongodb/hnsw | 564.04 | 10035.67 | 17.792 | self win |
| agg_multi | mongodb_plain/exact | 564.04 | 9711.54 | 17.218 | self win |
| agg_multi | mysql/exact | 564.04 | 2587.17 | 4.587 | self win |
| agg_multi | pgvector/exact | 564.04 | 2080.33 | 3.688 | self win |
| agg_multi | pgvector/hnsw | 564.04 | 2083.17 | 3.693 | self win |
| agg_multi | redis/exact | 564.04 | 6320.04 | 11.205 | self win |
| agg_multi | redis/hnsw | 564.04 | 6272.83 | 11.121 | self win |
| agg_multi | self/hnsw | 564.04 | 559.12 | 0.991 | 僅差 |
| agg_multi | self_nosql/exact | 564.04 | 656.83 | 1.165 | self win |
| agg_multi | sqlite_vec/exact | 564.04 | 1748.12 | 3.099 | self win |
| bulk_hybrid_k200 | lancedb/exact | 1618.96 | 5235.46 | 3.234 | self win |
| bulk_hybrid_k200 | lancedb/hnsw | 1618.96 | 3683.92 | 2.275 | self win |
| bulk_hybrid_k200 | mongodb/exact | 1618.96 | 11078.46 | 6.843 | self win |
| bulk_hybrid_k200 | mongodb/hnsw | 1618.96 | 10178.58 | 6.287 | self win |
| bulk_hybrid_k200 | pgvector/exact | 1618.96 | 4636.67 | 2.864 | self win |
| bulk_hybrid_k200 | pgvector/hnsw | 1618.96 | 4592.25 | 2.837 | self win |
| bulk_hybrid_k200 | redis/exact | 1618.96 | 1954.58 | 1.207 | self win |
| bulk_hybrid_k200 | redis/hnsw | 1618.96 | 2336.33 | 1.443 | self win |
| bulk_hybrid_k200 | self/hnsw | 1618.96 | 1529.75 | 0.945 | 僅差 |
| bulk_hybrid_k200 | self_nosql/exact | 1618.96 | 1789.29 | 1.105 | self win |
| bulk_hybrid_k200 | sqlite_vec/exact | 1618.96 | 6438.25 | 3.977 | self win |
| bulk_knn_k1000 | elasticsearch/exact | 1436.92 | 14906.50 | 10.374 | self win |
| bulk_knn_k1000 | elasticsearch/hnsw | 1436.92 | 14415.08 | 10.032 | self win |
| bulk_knn_k1000 | lancedb/exact | 1436.92 | 6158.17 | 4.286 | self win |
| bulk_knn_k1000 | lancedb/hnsw | 1436.92 | 3499.92 | 2.436 | self win |
| bulk_knn_k1000 | mongodb/exact | 1436.92 | 9784.46 | 6.809 | self win |
| bulk_knn_k1000 | mongodb/hnsw | 1436.92 | 11445.62 | 7.965 | self win |
| bulk_knn_k1000 | pgvector/exact | 1436.92 | 4501.00 | 3.132 | self win |
| bulk_knn_k1000 | pgvector/hnsw | 1436.92 | 4669.62 | 3.250 | self win |
| bulk_knn_k1000 | qdrant/exact | 1436.92 | 12339.04 | 8.587 | self win |
| bulk_knn_k1000 | qdrant/hnsw | 1436.92 | 11808.12 | 8.218 | self win |
| bulk_knn_k1000 | redis/exact | 1436.92 | 8451.21 | 5.881 | self win |
| bulk_knn_k1000 | redis/hnsw | 1436.92 | 8799.75 | 6.124 | self win |
| bulk_knn_k1000 | self/hnsw | 1436.92 | 1310.12 | 0.912 | 僅差 |
| bulk_knn_k1000 | self_nosql/exact | 1436.92 | 1752.21 | 1.219 | self win |
| bulk_knn_k1000 | sqlite_vec/exact | 1436.92 | 17737.79 | 12.344 | self win |
| bulk_knn_k200 | elasticsearch/exact | 735.00 | 5433.25 | 7.392 | self win |
| bulk_knn_k200 | elasticsearch/hnsw | 735.00 | 3820.50 | 5.198 | self win |
| bulk_knn_k200 | lancedb/exact | 735.00 | 4262.17 | 5.799 | self win |
| bulk_knn_k200 | lancedb/hnsw | 735.00 | 2457.54 | 3.344 | self win |
| bulk_knn_k200 | mongodb/exact | 735.00 | 3623.33 | 4.930 | self win |
| bulk_knn_k200 | mongodb/hnsw | 735.00 | 3352.29 | 4.561 | self win |
| bulk_knn_k200 | pgvector/exact | 735.00 | 3797.67 | 5.167 | self win |
| bulk_knn_k200 | pgvector/hnsw | 735.00 | 3617.13 | 4.921 | self win |
| bulk_knn_k200 | qdrant/exact | 735.00 | 3035.25 | 4.130 | self win |
| bulk_knn_k200 | qdrant/hnsw | 735.00 | 2891.96 | 3.935 | self win |
| bulk_knn_k200 | redis/exact | 735.00 | 2566.46 | 3.492 | self win |
| bulk_knn_k200 | redis/hnsw | 735.00 | 2667.50 | 3.629 | self win |
| bulk_knn_k200 | self/hnsw | 735.00 | 571.42 | 0.777 | self loss |
| bulk_knn_k200 | self_nosql/exact | 735.00 | 946.33 | 1.288 | self win |
| bulk_knn_k200 | sqlite_vec/exact | 735.00 | 5304.67 | 7.217 | self win |
| bulk_knn_where_k200 | elasticsearch/exact | 407.75 | 3654.17 | 8.962 | self win |
| bulk_knn_where_k200 | elasticsearch/hnsw | 407.75 | 3569.04 | 8.753 | self win |
| bulk_knn_where_k200 | lancedb/exact | 407.75 | 4508.37 | 11.057 | self win |
| bulk_knn_where_k200 | lancedb/hnsw | 407.75 | 3317.38 | 8.136 | self win |
| bulk_knn_where_k200 | mongodb/exact | 407.75 | 3216.71 | 7.889 | self win |
| bulk_knn_where_k200 | mongodb/hnsw | 407.75 | 3227.54 | 7.915 | self win |
| bulk_knn_where_k200 | pgvector/exact | 407.75 | 2439.33 | 5.982 | self win |
| bulk_knn_where_k200 | pgvector/hnsw | 407.75 | 2469.37 | 6.056 | self win |
| bulk_knn_where_k200 | qdrant/exact | 407.75 | 3161.71 | 7.754 | self win |
| bulk_knn_where_k200 | qdrant/hnsw | 407.75 | 3085.67 | 7.568 | self win |
| bulk_knn_where_k200 | redis/exact | 407.75 | 2381.63 | 5.841 | self win |
| bulk_knn_where_k200 | redis/hnsw | 407.75 | 2511.42 | 6.159 | self win |
| bulk_knn_where_k200 | self/hnsw | 407.75 | 2955.21 | 7.248 | self win |
| bulk_knn_where_k200 | self_nosql/exact | 407.75 | 545.79 | 1.339 | self win |
| bulk_knn_where_k200 | sqlite_vec/exact | 407.75 | 5959.79 | 14.616 | self win |
| group_by_having | elasticsearch/exact | 74.58 | 1049.58 | 14.073 | self win |
| group_by_having | elasticsearch/hnsw | 74.58 | 621.25 | 8.330 | self win |
| group_by_having | lancedb/exact | 74.58 | 6426.79 | 86.170 | self win |
| group_by_having | lancedb/hnsw | 74.58 | 6389.21 | 85.666 | self win |
| group_by_having | mongodb/exact | 74.58 | 8150.29 | 109.278 | self win |
| group_by_having | mongodb/hnsw | 74.58 | 8147.92 | 109.246 | self win |
| group_by_having | mongodb_plain/exact | 74.58 | 3990.92 | 53.510 | self win |
| group_by_having | mysql/exact | 74.58 | 11549.92 | 154.860 | self win |
| group_by_having | pgvector/exact | 74.58 | 3055.75 | 40.971 | self win |
| group_by_having | pgvector/hnsw | 74.58 | 3049.12 | 40.882 | self win |
| group_by_having | redis/exact | 74.58 | 6512.42 | 87.318 | self win |
| group_by_having | redis/hnsw | 74.58 | 6359.29 | 85.265 | self win |
| group_by_having | self/hnsw | 74.58 | 81.21 | 1.089 | 僅差 |
| group_by_having | self_nosql/exact | 74.58 | 182.33 | 2.445 | self win |
| group_by_having | sqlite_vec/exact | 74.58 | 2981.29 | 39.973 | self win |
| hybrid_rrf | lancedb/exact | 1561.83 | 3866.37 | 2.476 | self win |
| hybrid_rrf | lancedb/hnsw | 1561.83 | 2297.46 | 1.471 | self win |
| hybrid_rrf | mongodb/exact | 1561.83 | 5577.83 | 3.571 | self win |
| hybrid_rrf | mongodb/hnsw | 1561.83 | 3974.25 | 2.545 | self win |
| hybrid_rrf | pgvector/exact | 1561.83 | 4123.21 | 2.640 | self win |
| hybrid_rrf | pgvector/hnsw | 1561.83 | 4064.50 | 2.602 | self win |
| hybrid_rrf | redis/exact | 1561.83 | 1070.04 | 0.685 | 僅差 |
| hybrid_rrf | redis/hnsw | 1561.83 | 1274.00 | 0.816 | 僅差 |
| hybrid_rrf | self/hnsw | 1561.83 | 1410.79 | 0.903 | self loss |
| hybrid_rrf | self_nosql/exact | 1561.83 | 1682.29 | 1.077 | self win |
| hybrid_rrf | sqlite_vec/exact | 1561.83 | 4033.46 | 2.583 | self win |
| ingest_single_stmt | elasticsearch/exact | 176.98 | 638.26 | 3.606 | self loss |
| ingest_single_stmt | elasticsearch/hnsw | 176.98 | 695.74 | 3.931 | self loss |
| ingest_single_stmt | lancedb/exact | 176.98 | 781.20 | 4.414 | self loss |
| ingest_single_stmt | lancedb/hnsw | 176.98 | 716.01 | 4.046 | self loss |
| ingest_single_stmt | mongodb/exact | 176.98 | 955.96 | 5.401 | self loss |
| ingest_single_stmt | mongodb/hnsw | 176.98 | 1216.16 | 6.872 | self loss |
| ingest_single_stmt | mongodb_plain/exact | 176.98 | 3165.86 | 17.888 | self loss |
| ingest_single_stmt | mysql/exact | 176.98 | 826.89 | 4.672 | self loss |
| ingest_single_stmt | pgvector/exact | 176.98 | 1184.88 | 6.695 | self loss |
| ingest_single_stmt | pgvector/hnsw | 176.98 | 594.06 | 3.357 | self loss |
| ingest_single_stmt | qdrant/exact | 176.98 | 1364.71 | 7.711 | self loss |
| ingest_single_stmt | qdrant/hnsw | 176.98 | 564.94 | 3.192 | self loss |
| ingest_single_stmt | redis/exact | 176.98 | 3667.42 | 20.722 | self loss |
| ingest_single_stmt | redis/hnsw | 176.98 | 3539.74 | 20.001 | self loss |
| ingest_single_stmt | self/hnsw | 176.98 | 180.86 | 1.022 | 僅差 |
| ingest_single_stmt | self_nosql/exact | 176.98 | 149.49 | 0.845 | 僅差 |
| ingest_single_stmt | sqlite_vec/exact | 176.98 | 1772.99 | 10.018 | self loss |
| mode_precision | self/hnsw | 538.08 | 502.50 | 0.934 | 僅差 |
| mode_precision | self_nosql/exact | 538.08 | 666.12 | 1.238 | self win |
| mode_recall | self/hnsw | 535.96 | 426.62 | 0.796 | self loss |
| mode_recall | self_nosql/exact | 535.96 | 673.21 | 1.256 | self win |
| point_where | elasticsearch/exact | 236.92 | 1682.37 | 7.101 | self win |
| point_where | elasticsearch/hnsw | 236.92 | 1454.08 | 6.138 | self win |
| point_where | lancedb/exact | 236.92 | 3453.29 | 14.576 | self win |
| point_where | lancedb/hnsw | 236.92 | 2121.46 | 8.954 | self win |
| point_where | mongodb/exact | 236.92 | 1890.17 | 7.978 | self win |
| point_where | mongodb/hnsw | 236.92 | 2557.21 | 10.794 | self win |
| point_where | pgvector/exact | 236.92 | 2126.04 | 8.974 | self win |
| point_where | pgvector/hnsw | 236.92 | 2128.54 | 8.984 | self win |
| point_where | qdrant/exact | 236.92 | 619.71 | 2.616 | self win |
| point_where | qdrant/hnsw | 236.92 | 640.96 | 2.705 | self win |
| point_where | redis/exact | 236.92 | 822.83 | 3.473 | self win |
| point_where | redis/hnsw | 236.92 | 918.71 | 3.878 | self win |
| point_where | self/hnsw | 236.92 | 2207.88 | 9.319 | self win |
| point_where | self_nosql/exact | 236.92 | 354.54 | 1.496 | self win |
| point_where | sqlite_vec/exact | 236.92 | 2076.13 | 8.763 | self win |
| rls_isolation | elasticsearch/exact | 166.08 | 665.29 | 4.006 | self win |
| rls_isolation | elasticsearch/hnsw | 166.08 | 447.38 | 2.694 | self win |
| rls_isolation | lancedb/exact | 166.08 | 381.50 | 2.297 | self win |
| rls_isolation | lancedb/hnsw | 166.08 | 372.54 | 2.243 | self win |
| rls_isolation | mongodb/exact | 166.08 | 2355.13 | 14.180 | self win |
| rls_isolation | mongodb/hnsw | 166.08 | 2235.46 | 13.460 | self win |
| rls_isolation | mongodb_plain/exact | 166.08 | 2299.04 | 13.843 | self win |
| rls_isolation | mysql/exact | 166.08 | 1973.75 | 11.884 | self win |
| rls_isolation | pgvector/exact | 166.08 | 1848.00 | 11.127 | self win |
| rls_isolation | pgvector/hnsw | 166.08 | 1773.83 | 10.680 | self win |
| rls_isolation | qdrant/exact | 166.08 | 1901.04 | 11.446 | self win |
| rls_isolation | qdrant/hnsw | 166.08 | 1538.79 | 9.265 | self win |
| rls_isolation | redis/exact | 166.08 | 669.42 | 4.031 | self win |
| rls_isolation | redis/hnsw | 166.08 | 655.37 | 3.946 | self win |
| rls_isolation | self/hnsw | 166.08 | 162.92 | 0.981 | 僅差 |
| rls_isolation | self_nosql/exact | 166.08 | 266.46 | 1.604 | self win |
| rls_isolation | sqlite_vec/exact | 166.08 | 1049.21 | 6.317 | self win |
| scan_where_nosort_k500 | elasticsearch/exact | 447.50 | 6986.92 | 15.613 | self win |
| scan_where_nosort_k500 | elasticsearch/hnsw | 447.50 | 5832.79 | 13.034 | self win |
| scan_where_nosort_k500 | lancedb/exact | 447.50 | 1357.67 | 3.034 | self win |
| scan_where_nosort_k500 | lancedb/hnsw | 447.50 | 1351.88 | 3.021 | self win |
| scan_where_nosort_k500 | mongodb/exact | 447.50 | 1173.21 | 2.622 | self win |
| scan_where_nosort_k500 | mongodb/hnsw | 447.50 | 1192.87 | 2.666 | self win |
| scan_where_nosort_k500 | mongodb_plain/exact | 447.50 | 2299.63 | 5.139 | self win |
| scan_where_nosort_k500 | mysql/exact | 447.50 | 1110.75 | 2.482 | self win |
| scan_where_nosort_k500 | pgvector/exact | 447.50 | 1064.00 | 2.378 | self win |
| scan_where_nosort_k500 | pgvector/hnsw | 447.50 | 1076.83 | 2.406 | self win |
| scan_where_nosort_k500 | qdrant/exact | 447.50 | 4452.75 | 9.950 | self win |
| scan_where_nosort_k500 | qdrant/hnsw | 447.50 | 4566.83 | 10.205 | self win |
| scan_where_nosort_k500 | redis/exact | 447.50 | 4223.67 | 9.438 | self win |
| scan_where_nosort_k500 | redis/hnsw | 447.50 | 4223.62 | 9.438 | self win |
| scan_where_nosort_k500 | self/hnsw | 447.50 | 461.25 | 1.031 | 僅差 |
| scan_where_nosort_k500 | self_nosql/exact | 447.50 | 642.46 | 1.436 | self win |
| scan_where_nosort_k500 | sqlite_vec/exact | 447.50 | 198.21 | 0.443 | self loss |
| udf_call | self/hnsw | 543.96 | 419.92 | 0.772 | self loss |
| vector_knn | elasticsearch/exact | 537.04 | 3736.08 | 6.957 | self win |
| vector_knn | elasticsearch/hnsw | 537.04 | 1735.87 | 3.232 | self win |
| vector_knn | lancedb/exact | 537.04 | 3158.58 | 5.881 | self win |
| vector_knn | lancedb/hnsw | 537.04 | 1561.62 | 2.908 | self win |
| vector_knn | mongodb/exact | 537.04 | 2677.13 | 4.985 | self win |
| vector_knn | mongodb/hnsw | 537.04 | 1492.50 | 2.779 | self win |
| vector_knn | pgvector/exact | 537.04 | 3354.71 | 6.247 | self win |
| vector_knn | pgvector/hnsw | 537.04 | 3193.25 | 5.946 | self win |
| vector_knn | qdrant/exact | 537.04 | 739.88 | 1.378 | self win |
| vector_knn | qdrant/hnsw | 537.04 | 700.25 | 1.304 | 僅差 |
| vector_knn | redis/exact | 537.04 | 991.71 | 1.847 | self win |
| vector_knn | redis/hnsw | 537.04 | 1087.67 | 2.025 | self win |
| vector_knn | self/hnsw | 537.04 | 431.33 | 0.803 | self loss |
| vector_knn | self_nosql/exact | 537.04 | 667.42 | 1.243 | self win |
| vector_knn | sqlite_vec/exact | 537.04 | 2500.00 | 4.655 | self win |
| vector_knn_where | elasticsearch/exact | 236.92 | 1682.37 | 7.101 | self win |
| vector_knn_where | elasticsearch/hnsw | 236.92 | 1454.08 | 6.138 | self win |
| vector_knn_where | lancedb/exact | 236.92 | 3453.29 | 14.576 | self win |
| vector_knn_where | lancedb/hnsw | 236.92 | 2121.46 | 8.954 | self win |
| vector_knn_where | mongodb/exact | 236.92 | 1890.17 | 7.978 | self win |
| vector_knn_where | mongodb/hnsw | 236.92 | 2557.21 | 10.794 | self win |
| vector_knn_where | pgvector/exact | 236.92 | 2126.04 | 8.974 | self win |
| vector_knn_where | pgvector/hnsw | 236.92 | 2128.54 | 8.984 | self win |
| vector_knn_where | qdrant/exact | 236.92 | 619.71 | 2.616 | self win |
| vector_knn_where | qdrant/hnsw | 236.92 | 640.96 | 2.705 | self win |
| vector_knn_where | redis/exact | 236.92 | 822.83 | 3.473 | self win |
| vector_knn_where | redis/hnsw | 236.92 | 918.71 | 3.878 | self win |
| vector_knn_where | self/hnsw | 236.92 | 2207.88 | 9.319 | self win |
| vector_knn_where | self_nosql/exact | 236.92 | 354.54 | 1.496 | self win |
| vector_knn_where | sqlite_vec/exact | 236.92 | 2076.13 | 8.763 | self win |
| where_compound_count | elasticsearch/exact | 98.08 | 889.92 | 9.073 | self win |
| where_compound_count | elasticsearch/hnsw | 98.08 | 522.29 | 5.325 | self win |
| where_compound_count | lancedb/exact | 98.08 | 605.75 | 6.176 | self win |
| where_compound_count | lancedb/hnsw | 98.08 | 600.63 | 6.124 | self win |
| where_compound_count | mongodb/exact | 98.08 | 3307.00 | 33.716 | self win |
| where_compound_count | mongodb/hnsw | 98.08 | 3336.38 | 34.015 | self win |
| where_compound_count | mongodb_plain/exact | 98.08 | 2708.71 | 27.616 | self win |
| where_compound_count | mysql/exact | 98.08 | 3585.50 | 36.555 | self win |
| where_compound_count | pgvector/exact | 98.08 | 1534.62 | 15.646 | self win |
| where_compound_count | pgvector/hnsw | 98.08 | 1473.54 | 15.023 | self win |
| where_compound_count | qdrant/exact | 98.08 | 7277.96 | 74.201 | self win |
| where_compound_count | qdrant/hnsw | 98.08 | 7243.75 | 73.852 | self win |
| where_compound_count | redis/exact | 98.08 | 1135.12 | 11.573 | self win |
| where_compound_count | redis/hnsw | 98.08 | 1041.21 | 10.615 | self win |
| where_compound_count | self/hnsw | 98.08 | 113.37 | 1.156 | self win |
| where_compound_count | sqlite_vec/exact | 98.08 | 1366.87 | 13.936 | self win |
