#
# Tests measure the WAL pressure's performance under different workloads.
#
import statistics
import threading
import time
import timeit
from typing import Callable

import pytest
from batch_others.test_backpressure import pg_cur
from fixtures.benchmark_fixture import MetricReport, NeonBenchmarker
from fixtures.compare_fixtures import NeonCompare, PgCompare, VanillaCompare
from fixtures.log_helper import log
from fixtures.neon_fixtures import DEFAULT_BRANCH_NAME, NeonEnvBuilder, PgBin

from performance.test_perf_pgbench import (get_durations_matrix, get_scales_matrix)


@pytest.fixture(
    params=["vanilla", "neon_off_15MB", "neon_off_500MB", "neon_on_15MB", "neon_on_500MB"])
# This fixture constructs multiple `PgCompare` interfaces using a builder pattern.
# The builder parameters are encoded in the fixture's param.
# For example, to build a `NeonCompare` interface, the corresponding fixture's param should have
# a format of `neon_{safekeepers_enable_fsync}_{max_replication_apply_lag}`.
# Note that, here "_" is used to separate builder parameters.
def pg_compare(request) -> PgCompare:
    x = request.param.split("_")

    if x[0] == "vanilla":
        # `VanillaCompare` interface
        fixture = request.getfixturevalue("vanilla_compare")
        assert isinstance(fixture, VanillaCompare)

        return fixture
    else:
        assert len(x) == 3, f"request param ({request.param}) should have a format of \
        `neon_{{safekeepers_enable_fsync}}_{{max_replication_apply_lag}}`"

        # `NeonCompare` interface
        neon_env_builder = request.getfixturevalue("neon_env_builder")
        assert isinstance(neon_env_builder, NeonEnvBuilder)

        zenbenchmark = request.getfixturevalue("zenbenchmark")
        assert isinstance(zenbenchmark, NeonBenchmarker)

        pg_bin = request.getfixturevalue("pg_bin")
        assert isinstance(pg_bin, PgBin)

        neon_env_builder.safekeepers_enable_fsync = x[1] == "on"

        env = neon_env_builder.init_start()
        env.neon_cli.create_branch("empty", ancestor_branch_name=DEFAULT_BRANCH_NAME)

        branch_name = request.node.name
        return NeonCompare(zenbenchmark,
                           env,
                           pg_bin,
                           branch_name,
                           config_lines=[f"max_replication_write_lag={x[2]}"])


def start_heavy_write_workload(env: PgCompare, n_tables: int, scale: int, num_iters: int):
    """Start an intensive write workload across multiple tables.

    ## Single table workload:
    At each step, insert new `new_rows_each_update` rows.
    The variable `new_rows_each_update` is equal to `scale * 100_000`.
    The number of steps is determined by `num_iters` variable."""
    new_rows_each_update = scale * 100_000

    def start_single_table_workload(table_id: int):
        for _ in range(num_iters):
            with pg_cur(env.pg) as cur:
                cur.execute(
                    f"INSERT INTO t{table_id} SELECT FROM generate_series(1,{new_rows_each_update})"
                )

    with env.record_duration("run_duration"):
        threads = [
            threading.Thread(target=start_single_table_workload, args=(i, ))
            for i in range(n_tables)
        ]

        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()


@pytest.mark.parametrize("n_tables", [10])
@pytest.mark.parametrize("scale", [5])
@pytest.mark.parametrize("num_iters", [10])
def test_heavy_write_workload(pg_compare: PgCompare, n_tables: int, scale: int, num_iters: int):
    env = pg_compare

    # Initializes test tables
    with pg_cur(env.pg) as cur:
        for i in range(n_tables):
            cur.execute(
                f"CREATE TABLE t{i}(key serial primary key, t text default 'foooooooooooooooooooooooooooooooooooooooooooooooooooo')"
            )

    workload_thread = threading.Thread(target=start_heavy_write_workload,
                                       args=(env, n_tables, scale, num_iters))
    workload_thread.start()

    record_thread = threading.Thread(target=record_lsn_write_lag,
                                     args=(env, lambda: workload_thread.is_alive()))
    record_thread.start()

    record_read_latency(env, lambda: workload_thread.is_alive(), "SELECT * from t0 where key = 1")
    workload_thread.join()
    record_thread.join()


def start_pgbench_simple_update_workload(env: PgCompare, scale: int, duration: int):
    with env.record_duration("run_duration"):
        env.pg_bin.run_capture(['pgbench', f'-s{scale}', '-i', '-Igvp', env.pg.connstr()])
        env.flush()

        env.pg_bin.run_capture([
            'pgbench',
            '-j10',
            '-c10',
            '-N',
            f'-T{duration}',
            '-Mprepared',
            env.pg.connstr(options="-csynchronous_commit=off")
        ])
        env.flush()


@pytest.mark.parametrize("scale", get_scales_matrix(50))
@pytest.mark.parametrize("duration", get_durations_matrix())
def test_pgbench_simple_update_workload(pg_compare: PgCompare, scale: int, duration: int):
    env = pg_compare

    # create pgbench tables
    env.pg_bin.run_capture(['pgbench', f'-s{scale}', '-i', '-Idt', env.pg.connstr()])
    env.flush()

    workload_thread = threading.Thread(target=start_pgbench_simple_update_workload,
                                       args=(env, scale, duration))
    workload_thread.start()

    record_thread = threading.Thread(target=record_lsn_write_lag,
                                     args=(env, lambda: workload_thread.is_alive()))
    record_thread.start()

    record_read_latency(env,
                        lambda: workload_thread.is_alive(),
                        "SELECT * from pgbench_accounts where aid = 1")
    workload_thread.join()
    record_thread.join()


def start_pgbench_intensive_initialization(env: PgCompare, scale: int):
    with env.record_duration("run_duration"):
        # Needs to increase the statement timeout (default: 120s) because the
        # initialization step can be slow with a large scale.
        env.pg_bin.run_capture([
            'pgbench',
            f'-s{scale}',
            '-i',
            '-Idtg',
            env.pg.connstr(options='-cstatement_timeout=180s')
        ])


@pytest.mark.parametrize("scale", get_scales_matrix(1000))
def test_pgbench_intensive_init_workload(pg_compare: PgCompare, scale: int):
    env = pg_compare
    with pg_cur(env.pg) as cur:
        cur.execute("CREATE TABLE foo as select generate_series(1,100000)")

    workload_thread = threading.Thread(target=start_pgbench_intensive_initialization,
                                       args=(env, scale))
    workload_thread.start()

    record_thread = threading.Thread(target=record_lsn_write_lag,
                                     args=(env, lambda: workload_thread.is_alive()))
    record_thread.start()

    record_read_latency(env, lambda: workload_thread.is_alive(), "SELECT count(*) from foo")
    workload_thread.join()
    record_thread.join()


def record_lsn_write_lag(env: PgCompare, run_cond: Callable[[], bool], pool_interval: float = 1.0):
    if not isinstance(env, NeonCompare):
        return

    lsn_write_lags = []

    with pg_cur(env.pg) as cur:
        cur.execute("CREATE EXTENSION neon")

        while run_cond():
            cur.execute('''
            select pg_wal_lsn_diff(pg_current_wal_flush_lsn(),received_lsn),
            pg_size_pretty(pg_wal_lsn_diff(pg_current_wal_flush_lsn(),received_lsn))
            from backpressure_lsns();
            ''')

            res = cur.fetchone()
            lsn_write_lags.append(res[0])

            log.info(f"received_lsn_lag = {res[1]}")

            time.sleep(pool_interval)

    env.zenbenchmark.record("lsn_write_lag_max",
                            max(lsn_write_lags) // 1024,
                            "kB",
                            MetricReport.LOWER_IS_BETTER)
    env.zenbenchmark.record("lsn_write_lag_avg",
                            statistics.mean(lsn_write_lags) // 1024,
                            "kB",
                            MetricReport.LOWER_IS_BETTER)
    env.zenbenchmark.record("lsn_write_lag_stdev",
                            statistics.stdev(lsn_write_lags) // 1024,
                            "kB",
                            MetricReport.LOWER_IS_BETTER)


def record_read_latency(env: PgCompare,
                        run_cond: Callable[[], bool],
                        read_query: str,
                        read_interval: float = 1.0):
    read_latencies = []
    while run_cond():
        t0 = timeit.default_timer()
        try:
            with pg_cur(env.pg) as cur:
                t1 = timeit.default_timer()
                cur.execute(read_query)
                t2 = timeit.default_timer()

                log.info(
                    f"Executed read query {read_query}, got {cur.fetchall()}, connection time {t1-t0}, read time {t2-t1}"
                )
                read_latencies.append(t2 - t0)
        except Exception as err:
            log.error(f"Got error when executing the read query: {err}")

        time.sleep(read_interval)

    env.zenbenchmark.record("read_latency_max",
                            max(read_latencies),
                            's',
                            MetricReport.LOWER_IS_BETTER)
    env.zenbenchmark.record("read_latency_avg",
                            statistics.mean(read_latencies),
                            's',
                            MetricReport.LOWER_IS_BETTER)
    env.zenbenchmark.record("read_latency_stdev",
                            statistics.stdev(read_latencies),
                            's',
                            MetricReport.LOWER_IS_BETTER)