import argparse
import json
import os
from pathlib import Path
import random
import tempfile
import unittest
from unittest.mock import patch
import sys
sys.path.insert(0,str(Path(__file__).resolve().parents[1]))
import fixture_set as fixtures
import evaluate_set as evaluation


class FlowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        root=os.environ.get('BACKRUN_TEST_SET')
        if not root: raise unittest.SkipTest('set BACKRUN_TEST_SET to real generated fixtures')
        cls.root=Path(root);cls.market=fixtures.read(cls.root/'incentivized/market.json')
        cls.states=fixtures.pool_states(cls.market,fixtures.read(cls.root/'incentivized/snapshot.json'))

    def test_liquidity_preferences_and_reproducibility(self):
        a,_=fixtures.liquidity_flow(self.market,self.states,random.Random(987))
        b,_=fixtures.liquidity_flow(self.market,self.states,random.Random(987))
        self.assertEqual(a,b)
        fixtures.validate_flow({'version':1,'duration_slots':2000,'orders':a},self.market)
        concentrated=sum(self.market['pools'][o['route'][0]['pool']]['kind']=='concentrated' for o in a)
        self.assertGreater(concentrated/len(a),.65);self.assertLess(concentrated/len(a),.95)
        self.assertLess(len({o['slot'] for o in a}),2000)
        self.assertGreater(len(a),len({o['slot'] for o in a}))

    def test_frenzy_direction_pairs_variation_and_alternative_paths(self):
        a,meta=fixtures.frenzy_flow(self.market,self.states,random.Random(876))
        self.assertEqual(a,fixtures.frenzy_flow(self.market,self.states,random.Random(876))[0])
        self.assertIn(len(meta['episodes']),[4,5]);self.assertIsNone(meta['stablecoin_assumption'])
        turns=[]
        for e in meta['episodes']:
            p=self.market['pools'][e['pool']];self.assertIn(0,p['mints'])
            self.assertTrue(any(q['mints']==p['mints'] and q['kind']=='concentrated' for q in self.market['pools']))
            turns.append((e['turn']-e['start'])/(e['end']-e['start']))
            for start,end,buying in [(e['start'],e['turn'],True),(e['turn'],e['end'],False)]:
                orders=[o for o in a if start<=o['slot']<end and o['route'][0]['pool']==e['pool']]
                aligned=sum((p['mints'][0 if o['route'][0]['a_to_b'] else 1]==0)==buying for o in orders)
                self.assertGreater(aligned/len(orders),.7)
        self.assertGreater(max(turns)-min(turns),.1)
        fixtures.validate_flow({'version':1,'duration_slots':2000,'orders':a},self.market)

    def test_original_flow_and_initial_market_preserved(self):
        source=os.environ.get('BACKRUN_ORIGINAL_FIXTURE')
        if not source:self.skipTest('original fixture path not provided')
        for name in ['market.json','flow.json','snapshot.json','users.json','provenance.json']:
            self.assertEqual(fixtures.digest(Path(source)/name),fixtures.digest(self.root/'incentivized'/name))
        for regime in fixtures.REGIMES:
            self.assertEqual(fixtures.digest(self.root/regime/'market.json'),fixtures.digest(Path(source)/'market.json'))
            self.assertEqual(fixtures.digest(self.root/regime/'snapshot.json'),fixtures.digest(Path(source)/'snapshot.json'))

    def test_order_constraints(self):
        orders,_=fixtures.liquidity_flow(self.market,self.states,random.Random(12))
        orders[0]['slot']=2001
        with self.assertRaises(ValueError):fixtures.validate_flow({'version':1,'duration_slots':2000,'orders':orders},self.market)


class CoordinatorTests(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory();self.root=Path(self.tmp.name);self.release=self.root/'release';self.release.mkdir()
        self.artifact=self.root/'artifact';self.artifact.mkdir()
        for name,content in [('run.sh','#!/bin/sh\nexec ./bot\n'),('bot','identical-native-artifact'),('manifest.json','{"version":2,"executable":"bot","programs":[],"scratch":[]}')]:
            p=self.artifact/name;p.write_text(content);p.chmod(0o700 if name!='manifest.json' else 0o600)
        scenarios=[]
        for n,regime in enumerate(fixtures.REGIMES,1):
            sid=f'scenario-{n:02}';task=self.release/'tasks'/sid;task.mkdir(parents=True)
            (task/'instruction.md').write_text('Build one bot for three private sessions.')
            (task/'task.toml').write_text('schema_version="1.4"')
            scenarios.append({'id':sid,'regime':regime,'task':'tasks/'+sid,'fixture_hash':sid})
        self.meta={'kind':'private-scenario-set','protocol':'continuous-arrival-v1','aggregation':None,
                   'scenarios':scenarios,'fixture_set_hash':'set','runtime_binary_sha256':'runtime','images':{},
                   'coordinator_sha256':evaluation.digest(evaluation.__file__),
                   'task_files':{str(p.relative_to(self.release)):evaluation.digest(p) for p in self.release.rglob('*') if p.is_file()}}
        evaluation.write(self.release/'release.json',self.meta)
        self.args=argparse.Namespace(release=self.release,jobs_dir=self.root/'jobs',job_name='run',calibration=False,artifact=None,
                                    agent='test-agent',model='test-model',effort='high',dry_run=False)
        self.calls=[];self.fail_at=None;self.disqualify_at=None;self.copy_failure=False

    def tearDown(self):self.tmp.cleanup()

    def fake_run(self,cmd,**kwargs):
        if cmd[0]=='docker':return
        self.calls.append(cmd)
        get=lambda name:cmd[cmd.index(name)+1]
        job_name=get('--job-name');sid=job_name.split('-infra-retry-')[0];task=Path(get('--path'));job=Path(get('--jobs-dir'))/job_name/'trial'
        if get('--agent')=='oracle':
            self.assertNotIn('--model',cmd)
            source=task/'solution/artifact'
            self.assertEqual(evaluation.artifact_files(source),evaluation.artifact_files(self.artifact))
            self.assertTrue((task/'solution/solve.sh').stat().st_mode & 0o005)
            self.assertTrue((source/'manifest.json').stat().st_mode & 0o004)
            (job/'agent').mkdir(parents=True)  # Harbor writes exit-code.txt only on failure
            if self.copy_failure:(job/'agent/exit-code.txt').write_text('126')
        else:source=self.artifact
        import shutil
        shutil.copytree(source,job/'artifacts/app/bot')
        (job/'verifier').mkdir()
        if self.fail_at==sid:
            evaluation.write(job/'verifier/infrastructure-error.json',{'error':'test clock failure'})
            raise RuntimeError('test infrastructure failure')
        score={'valid':True,'reward':{'scenario-01':1.0,'scenario-02':-2.0,'scenario-03':3.0}[sid]}
        result={'valid':True,'fixture_hash':sid,'completed_slots':2000,'timing':{'coalesced_slots':0,'slots':[]},
                'artifact_hash':'artifact','hardware':{'fingerprint':'hardware'},'runtime_binary_sha256':'runtime',
                'participants':[{'score':score}],'user_executions':1,'user_failures':0}
        if self.disqualify_at==sid:result.update(valid=False,terminal_disqualification='invalid native executable')
        evaluation.write(job/'verifier/report.json',result)

    def test_one_development_three_evaluations_same_artifact_separate_signed_scores(self):
        with patch.object(evaluation.subprocess,'run',side_effect=self.fake_run):evaluation.evaluate(self.args)
        self.assertEqual([c[c.index('--agent')+1] for c in self.calls],['test-agent','oracle','oracle'])
        report=evaluation.read(self.root/'jobs/run/report.json')
        self.assertEqual(report['reward_by_scenario'],{'scenario-01':1.,'scenario-02':-2.,'scenario-03':3.})
        self.assertIsNone(report['aggregation']);self.assertEqual(report['status'],'complete')
        with self.assertRaises(FileExistsError):evaluation.evaluate(self.args)

    def test_infrastructure_failure_preserves_partial_results_without_zero_score(self):
        self.fail_at='scenario-02'
        with patch.object(evaluation.subprocess,'run',side_effect=self.fake_run):
            with self.assertRaises(RuntimeError):evaluation.evaluate(self.args)
        report=evaluation.read(self.root/'jobs/run/report.json')
        self.assertEqual(report['status'],'incomplete');self.assertEqual(report['reward_by_scenario'],{'scenario-01':1.})
        self.assertEqual(len(self.calls),2)

    def test_disqualification_is_distinct_from_infrastructure(self):
        self.disqualify_at='scenario-01'
        with patch.object(evaluation.subprocess,'run',side_effect=self.fake_run):evaluation.evaluate(self.args)
        report=evaluation.read(self.root/'jobs/run/report.json')
        self.assertEqual(report['status'],'disqualified');self.assertEqual(report['not_run'],['scenario-02','scenario-03'])

    def test_explicit_recovery_reuses_success_and_never_repeats_development(self):
        self.fail_at='scenario-02'
        with patch.object(evaluation.subprocess,'run',side_effect=self.fake_run):
            with self.assertRaises(RuntimeError):evaluation.evaluate(self.args)
            self.fail_at=None;self.calls=[];self.args.resume=True;self.args.retry_infrastructure=True
            evaluation.evaluate(self.args)
        self.assertEqual(len(self.calls),2)
        self.assertTrue(all(c[c.index('--agent')+1]=='oracle' for c in self.calls))
        self.assertIn('scenario-02-infra-retry-01',self.calls[0])
        report=evaluation.read(self.root/'jobs/run/report.json')
        self.assertEqual(report['status'],'complete');self.assertEqual(report['reward_by_scenario']['scenario-01'],1.)
        with patch.object(evaluation.subprocess,'run',side_effect=self.fake_run):
            self.calls=[];evaluation.evaluate(self.args)
        self.assertEqual(self.calls,[])

    def test_recovery_rejects_multiple_scored_runs(self):
        with patch.object(evaluation.subprocess,'run',side_effect=self.fake_run):evaluation.evaluate(self.args)
        import shutil
        job=self.root/'jobs/run/harbor'
        shutil.copytree(job/'scenario-01',job/'scenario-01-infra-retry-01')
        self.args.resume=True
        with self.assertRaisesRegex(ValueError,'multiple scored runs'):evaluation.evaluate(self.args)

    def test_first_evaluation_failure_preserves_developed_artifact(self):
        self.fail_at='scenario-01'
        with patch.object(evaluation.subprocess,'run',side_effect=self.fake_run):
            with self.assertRaises(RuntimeError):evaluation.evaluate(self.args)
            report=evaluation.read(self.root/'jobs/run/report.json')
            self.assertEqual(report['artifact_files'],evaluation.artifact_files(self.artifact))
            self.fail_at=None;self.calls=[];self.args.resume=True;self.args.retry_infrastructure=True
            evaluation.evaluate(self.args)
        self.assertEqual(len(self.calls),3)
        self.assertTrue(all(c[c.index('--agent')+1]=='oracle' for c in self.calls))
        self.assertEqual(evaluation.read(self.root/'jobs/run/report.json')['status'],'complete')

    def test_copy_stage_failure_cannot_be_scored_as_candidate_failure(self):
        self.args.artifact=self.artifact;self.copy_failure=True
        with patch.object(evaluation.subprocess,'run',side_effect=self.fake_run):
            with self.assertRaisesRegex(RuntimeError,'trusted artifact-copy stage failed'):evaluation.evaluate(self.args)
        report=evaluation.read(self.root/'jobs/run/report.json')
        self.assertEqual(report['status'],'incomplete');self.assertEqual(report['reward_by_scenario'],{})

    def test_mutated_task_and_artifact_symlinks_rejected(self):
        (self.release/'tasks/scenario-01/instruction.md').write_text('changed')
        with self.assertRaises(ValueError):evaluation.verify_release(self.release)
        (self.artifact/'link').symlink_to(self.artifact/'manifest.json')
        with self.assertRaises(ValueError):evaluation.artifact_files(self.artifact)


if __name__=='__main__':unittest.main()
