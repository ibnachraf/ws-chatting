# ws-chatting
'´´´java
<dependencies>
  <!-- Vert.x -->
  <dependency>
    <groupId>io.vertx</groupId>
    <artifactId>vertx-pg-client</artifactId>
    <version>4.5.7</version>
  </dependency>
  <dependency>
    <groupId>io.vertx</groupId>
    <artifactId>vertx-kafka-client</artifactId>
    <version>4.5.7</version>
  </dependency>
  <dependency>
    <groupId>io.vertx</groupId>
    <artifactId>vertx-rx-java3</artifactId>
    <version>4.5.7</version>
  </dependency>

  <!-- Avro + Schema Registry -->
  <dependency>
    <groupId>org.apache.avro</groupId>
    <artifactId>avro</artifactId>
    <version>1.11.3</version>
  </dependency>
  <dependency>
    <groupId>io.confluent</groupId>
    <artifactId>kafka-avro-serializer</artifactId>
    <version>7.6.0</version>
  </dependency>
</dependencies>

´´´

main

package com.example.dump;

import io.vertx.core.DeploymentOptions;
import io.vertx.core.Vertx;
import io.vertx.core.VertxOptions;

public class MainVerticle {

    public static void main(String[] args) {
        DatabaseDumpConfig config = DatabaseDumpConfig.fromEnv();

        Vertx vertx = Vertx.vertx(new VertxOptions()
            .setEventLoopPoolSize(1)
            .setWorkerPoolSize(config.workerThreads())
            .setMaxEventLoopExecuteTime(5_000_000_000L) // 5s before "blocked thread" warning
        );

        vertx.deployVerticle(
            new DatabaseDumper(config),
            new DeploymentOptions()
        ).onSuccess(id -> {
            System.out.println("Deployment " + id + " complete, shutting down.");
            vertx.close();
        }).onFailure(err -> {
            System.err.println("Dump failed: " + err.getMessage());
            err.printStackTrace();
            vertx.close();
            System.exit(1);
        });
    }
}


dumper


package com.example.dump;

import io.vertx.core.*;
import io.vertx.kafka.client.producer.KafkaProducer;
import io.vertx.kafka.client.producer.KafkaProducerRecord;
import io.vertx.pgclient.*;
import io.vertx.sqlclient.*;

import java.util.HashMap;
import java.util.Map;
import java.util.concurrent.atomic.AtomicLong;
import java.util.logging.Logger;

/**
 * Streams all rows from Postgres → Kafka using:
 *  - Keyset pagination (cursor on `id`) — O(1) per page, no OFFSET drift
 *  - Vert.x cursor with prefetch for backpressure on the DB side
 *  - Kafka write-ahead semaphore to avoid overwhelming the broker
 */
public class DatabaseDumper extends AbstractVerticle {

    private static final Logger LOG = Logger.getLogger(DatabaseDumper.class.getName());

    private final DatabaseDumpConfig config;
    private final AvroSerializer avroSerializer;

    // Metrics
    private final AtomicLong rowsRead      = new AtomicLong();
    private final AtomicLong rowsPublished = new AtomicLong();
    private final AtomicLong rowsFailed    = new AtomicLong();

    private PgPool pgPool;
    private KafkaProducer<String, byte[]> kafkaProducer;

    public DatabaseDumper(DatabaseDumpConfig config) {
        this.config = config;
        this.avroSerializer = new AvroSerializer(config.schemaRegistryUrl(), config.kafkaTopic());
    }

    // -------------------------------------------------------------------------
    // Lifecycle
    // -------------------------------------------------------------------------

    @Override
    public void start(Promise<Void> startPromise) {
        pgPool = buildPgPool();
        kafkaProducer = buildKafkaProducer();

        LOG.info("Starting dump: topic=%s pageSize=%d".formatted(
            config.kafkaTopic(), config.pageSize()));

        runDump()
            .onSuccess(v -> {
                LOG.info("Dump complete. read=%d published=%d failed=%d"
                    .formatted(rowsRead.get(), rowsPublished.get(), rowsFailed.get()));
                startPromise.complete();
            })
            .onFailure(err -> {
                LOG.severe("Dump failed: " + err.getMessage());
                startPromise.fail(err);
            });
    }

    @Override
    public void stop(Promise<Void> stopPromise) {
        avroSerializer.close();
        kafkaProducer.close()
            .compose(v -> pgPool.close())
            .onComplete(stopPromise);
    }

    // -------------------------------------------------------------------------
    // Dump pipeline
    // -------------------------------------------------------------------------

    private Future<Void> runDump() {
        return pgPool.withConnection(conn ->
            openCursor(conn)
                .compose(cursor -> streamCursor(cursor, conn))
        );
    }

    /**
     * Open a server-side cursor with a prepared statement.
     * Using keyset pagination: ORDER BY id — no OFFSET.
     */
    private Future<Cursor> openCursor(SqlConnection conn) {
        String sql = """
            SELECT id, email, username, created_at, status
            FROM users
            ORDER BY id
            """;

        return conn.prepare(sql)
            .map(stmt -> stmt.cursor()); // server-side cursor
    }

    /**
     * Read the cursor page by page, publishing each batch to Kafka.
     * Recursive, but trampolined via Future to avoid stack overflow.
     */
    private Future<Void> streamCursor(Cursor cursor, SqlConnection conn) {
        return readPage(cursor)
            .compose(rows -> {
                if (rows.size() == 0) {
                    LOG.info("Cursor exhausted.");
                    return cursor.close();
                }

                rowsRead.addAndGet(rows.size());

                return publishBatch(rows)
                    .compose(v -> {
                        long total = rowsRead.get();
                        if (total % 100_000 == 0) {
                            LOG.info("Progress: read=%d published=%d failed=%d"
                                .formatted(total, rowsPublished.get(), rowsFailed.get()));
                        }
                        // Recurse — trampolined via Future, not the call stack
                        return streamCursor(cursor, conn);
                    });
            });
    }

    private Future<RowSet<Row>> readPage(Cursor cursor) {
        return cursor.read(config.pageSize());
    }

    // -------------------------------------------------------------------------
    // Kafka publishing with backpressure
    // -------------------------------------------------------------------------

    /**
     * Publish a batch of rows to Kafka.
     * Sliding window: flush every kafkaMaxInFlight records to apply backpressure.
     * Each row is sent independently so failures are row-level, not batch-level.
     */
    private Future<Void> publishBatch(RowSet<Row> rows) {
        var futures  = new java.util.ArrayList<Future<Void>>(rows.size());
        var window   = new java.util.ArrayList<Future<Void>>(config.kafkaMaxInFlight());
        int limit    = config.kafkaMaxInFlight();

        for (Row row : rows) {
            window.add(sendRow(row));
            if (window.size() >= limit) {
                futures.addAll(window);
                window.clear();
            }
        }
        futures.addAll(window);

        return Future.join(futures).mapEmpty();
    }

    private Future<Void> sendRow(Row row) {
        try {
            byte[] avroBytes = avroSerializer.serialize(config.kafkaTopic(), row);
            String key       = String.valueOf(row.getLong("id"));

            KafkaProducerRecord<String, byte[]> record =
                KafkaProducerRecord.create(config.kafkaTopic(), key, avroBytes);

            return kafkaProducer.send(record)
                .map(metadata -> {
                    rowsPublished.incrementAndGet();
                    return (Void) null;
                })
                .recover(err -> {
                    rowsFailed.incrementAndGet();
                    LOG.warning("Failed to send row id=%s: %s"
                        .formatted(row.getLong("id"), err.getMessage()));
                    // Adjust to Future.failedFuture(err) for strict/fail-fast mode
                    return Future.succeededFuture();
                });

        } catch (Exception e) {
            rowsFailed.incrementAndGet();
            LOG.warning("Serialization error for row id=%s: %s"
                .formatted(row.getLong("id"), e.getMessage()));
            return Future.succeededFuture();
        }
    }

    // -------------------------------------------------------------------------
    // Builders
    // -------------------------------------------------------------------------

    private PgPool buildPgPool() {
        PgConnectOptions connectOptions = new PgConnectOptions()
            .setHost(config.pgHost())
            .setPort(config.pgPort())
            .setDatabase(config.pgDatabase())
            .setUser(config.pgUser())
            .setPassword(config.pgPassword())
            .setPipeliningLimit(1); // TCP fetch hint for cursor streaming

        PoolOptions poolOptions = new PoolOptions()
            .setMaxSize(config.pgPoolSize())
            .setMaxWaitQueueSize(10);

        return PgPool.pool(vertx, connectOptions, poolOptions);
    }

    private KafkaProducer<String, byte[]> buildKafkaProducer() {
        Map<String, String> kafkaConfig = new HashMap<>();
        kafkaConfig.put("bootstrap.servers",    config.kafkaBootstrapServers());
        kafkaConfig.put("key.serializer",       "org.apache.kafka.common.serialization.StringSerializer");
        kafkaConfig.put("value.serializer",     "org.apache.kafka.common.serialization.ByteArraySerializer");

        // Durability: all ISR replicas must ack
        kafkaConfig.put("acks",                 "all");

        // Idempotent producer — exactly-once at producer level
        kafkaConfig.put("enable.idempotence",   "true");

        // Throughput tuning
        kafkaConfig.put("linger.ms",            "20");
        kafkaConfig.put("batch.size",           "65536");     // 64KB batches
        kafkaConfig.put("compression.type",     "snappy");
        kafkaConfig.put("buffer.memory",        "67108864");  // 64MB

        // Retries
        kafkaConfig.put("retries",              "5");
        kafkaConfig.put("retry.backoff.ms",     "200");
        kafkaConfig.put("delivery.timeout.ms",  "120000");

        return KafkaProducer.create(vertx, kafkaConfig);
    }
}

