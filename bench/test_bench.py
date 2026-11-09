"""Checks for failure accounting and report integrity; no external network."""
import json
import pathlib
import tempfile
import threading
import unittest
import urllib.error
import urllib.request
import bench

class Tests(unittest.TestCase):
    def test_fixed_routes_reject_private_and_unrelated_hosts(self):
        for value in [{'github.com':['127.0.0.1']},{'evil.test':['1.1.1.1']},{'github.com':[]}]:
            with self.assertRaises(ValueError):bench.options({'fixed_ips':value})
        mapping={'github.com':['140.82.116.4']}
        self.assertEqual(bench.options({'fixed_ips':mapping})['fixed_ips'],mapping)
        self.assertNotIn('fixed_ips',bench.options({'fixed_ips':None}))
    def test_server_reserves_a_single_loopback_listener(self):
        server=bench.LoopbackServer(('127.0.0.1',0),bench.Handler)
        try:
            with self.assertRaises(OSError):
                bench.LoopbackServer(('127.0.0.1',server.server_port),bench.Handler)
        finally:server.server_close()
    def test_experiments_change_one_flag_on_the_same_binary(self):
        value=bench.options({'feature':'range_memo','candidate':'build.exe'})
        self.assertEqual(value['baseline'],'build.exe')
        self.assertEqual(value['baseline_options'],{'segmented_downloads':True,'range_memo':False})
        self.assertEqual(value['candidate_options'],{'segmented_downloads':True,'range_memo':True})
        for feature in ['probe_reuse','range_memo','range_pipeline','adaptive_split','segmented_downloads']:
            value=bench.options({'feature':feature,'candidate':'build.exe'})
            a,b=value['baseline_options'],value['candidate_options']
            self.assertEqual([key for key in a if a[key]!=b[key]],[feature])

    def test_run_lock_prevents_competing_cli_bandwidth(self):
        with tempfile.TemporaryDirectory() as folder:
            path=pathlib.Path(folder)/'lock'
            first=bench.RunLease(path)
            try:
                with self.assertRaises(RuntimeError):bench.RunLease(path)
            finally:first.close()
            again=bench.RunLease(path);again.close()

    def test_cancellation_is_visible_to_a_separate_cli(self):
        with tempfile.TemporaryDirectory() as folder:
            path=pathlib.Path(folder)/'cancel'
            first,second=bench.CancelFlag(path),bench.CancelFlag(path)
            self.assertFalse(second.is_set())
            first.set()
            self.assertTrue(second.is_set())

    def test_route_order_balances_adjacent_rounds(self):
        self.assertEqual(bench.route_order(['a','b'],0),['a','b'])
        self.assertEqual(bench.route_order(['a','b'],1),['b','a'])
        self.assertEqual(bench.route_order(['a','b','c'],1),['c','b','a'])
    def test_failed_fast_response_cannot_improve_latency(self):
        rows = [dict(case='css',route='baseline',phase='cold',round=1,success=True,seconds=2,bps=10),
                dict(case='css',route='candidate',phase='cold',round=1,success=False,seconds=.001,bps=0,timeout=True)]
        summaries = bench.summary(rows)
        self.assertIsNone(summaries[1]['median_seconds'])
        self.assertEqual(summaries[1]['success_rate'],0)
        comparison = bench.paired(rows)[0]
        self.assertEqual(comparison['valid_pairs'],0)
        self.assertEqual(comparison['candidate_failures'],1)
        self.assertIsNone(comparison['median_speedup'])

    def test_pair_requires_same_round_case_and_phase(self):
        rows = [dict(case='raw',route='baseline',phase='cold',round=1,success=True,seconds=2,bps=10),
                dict(case='raw',route='candidate',phase='warm',round=1,success=True,seconds=1,bps=10)]
        self.assertEqual(bench.paired(rows),[])

    def test_partial_and_wrong_content_are_failures(self):
        with tempfile.TemporaryDirectory() as folder:
            file = pathlib.Path(folder)/'body'
            file.write_bytes(b'error page')
            reasons,_,_ = bench.validate({'status':200,'bytes':100,'sha256':'0'*64},file,200,{})
            self.assertIn('incomplete_or_wrong_length',reasons)
            self.assertIn('sha256_mismatch',reasons)
            reasons,_,_ = bench.validate({'status':206,'range':'0-9','total':20},file,206,{'content-range':'bytes 1-10/20'})
            self.assertIn('content_range_mismatch',reasons)

    def test_final_headers_strip_redirect_and_session_data(self):
        with tempfile.TemporaryDirectory() as folder:
            path = pathlib.Path(folder)/'headers'
            path.write_text('HTTP/1.1 302 Found\nLocation: https://example/?token=secret\n\nHTTP/1.1 200 OK\nSet-Cookie: secret\nETag: "ok"\n\n')
            self.assertEqual(bench.final_headers(path),{'etag':'"ok"'})

    def test_local_server_rejects_foreign_host_and_missing_token(self):
        server = bench.LoopbackServer(('127.0.0.1',0),bench.Handler)
        thread = threading.Thread(target=server.serve_forever,daemon=True); thread.start()
        url = 'http://127.0.0.1:%s'%server.server_port
        client = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        try:
            for request in [urllib.request.Request(url+'/api/current',headers={'Host':'evil.test'}),
                            urllib.request.Request(url+'/api/run',data=b'{}',method='POST')]:
                with self.assertRaises(urllib.error.HTTPError) as result:
                    client.open(request,timeout=2)
                self.assertEqual(result.exception.code,403)
            with client.open(url+'/api/info',timeout=2) as r:
                self.assertIn('cases',json.load(r))
        finally:
            server.shutdown();server.server_close();thread.join()

    def test_options_bound_workload(self):
        for value in ({'rounds':100},{'timeout':999},{'routes':['unknown']},{'cases':['../secret']}):
            with self.assertRaises(ValueError):
                bench.options(value)

if __name__=='__main__':
    unittest.main()
