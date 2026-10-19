"""Regression coverage for reusable curves, scoped edits and v1 upgrades."""
import copy
import json
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import patch

from fancontrollx.config import default_fan, save_config, validate_config
from fancontrollx.engine import ConfigConflict, Engine
from test_core import FixedHardware
import test_core


class WorkspaceTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / 'config.json'
        self.hw = FixedHardware()
        self.engine = Engine(self.hw, self.path, demo=True)
        self.addCleanup(self.engine.close)
        for f in self.engine.config['fans']:
            f.update(identified=True, device_kind='fan', response_s=0,
                     smoothing_s=0, hysteresis=0, step_up=100, step_down=100)
        self.first, self.second = [f['id'] for f in self.engine.config['fans'][:2]]

    def test_shared_geometry_keeps_temperature_sources_independent(self):
        id_ = self.engine.save_curve({'name': '我的曲线', 'points': [[20, 20], [100, 100]]})
        self.engine.patch_fan(self.first, {'curve_id': id_, 'sensor_ids': ['@cpu']})
        self.engine.patch_fan(self.second, {'curve_id': id_, 'sensor_ids': ['@gpu']})
        self.hw.cpu, self.hw.gpu_temp = 60, 40
        self.engine.tick(now=time.monotonic() + 2)
        outputs = self.engine.state()['outputs']
        self.assertEqual(outputs[self.first]['target'], 60)
        self.assertEqual(outputs[self.second]['target'], 40)
        curve = copy.deepcopy(self.engine.config['curves'][0])
        self.engine.save_curve({**curve, 'points': [[20, 60], [100, 60]]}, expected=curve)
        self.engine.tick(now=time.monotonic() + 4)
        self.assertEqual({o['target'] for id_, o in self.engine.state()['outputs'].items()
                          if id_ in (self.first, self.second)}, {60})
        self.assertEqual([f['sensor_ids'] for f in self.engine.config['fans'][:2]], [['@cpu'], ['@gpu']])

    def test_scoped_edit_does_not_overwrite_other_fan_or_stop_control(self):
        self.engine.activate(self.second)
        lease = self.engine.leases[self.second]
        expected = {'name': self.engine.config['fans'][0]['name']}
        self.engine.patch_fan(self.second, {'manual_duty': 77})
        self.engine.patch_fan(self.first, {'name': '任意名称'}, expected=expected)
        self.assertEqual(self.engine.config['fans'][1]['manual_duty'], 77)
        self.assertIs(self.engine.leases[self.second], lease)
        self.assertIn(self.second, self.engine.enabled)

    def test_card_order_persists_without_reacquiring_or_overwriting_fan_settings(self):
        self.engine.activate(self.second)
        lease = self.engine.leases[self.second]
        before = copy.deepcopy(self.engine.config['fans'])
        ids = [f['id'] for f in before]
        self.engine.patch_fan(self.first, {'manual_duty': 72})
        self.hw.writes.clear()
        self.engine.reorder_fans(list(reversed(ids)), expected=ids)
        self.assertEqual([f['id'] for f in self.engine.config['fans']], list(reversed(ids)))
        self.assertEqual(next(f for f in self.engine.config['fans'] if f['id'] == self.first)['manual_duty'], 72)
        self.assertIs(self.engine.leases[self.second], lease)
        self.assertEqual(self.hw.writes, [])
        self.assertEqual(json.loads(self.path.read_text()), self.engine.config)

    def test_invalid_and_conflicting_card_orders_preserve_config(self):
        before = copy.deepcopy(self.engine.config)
        ids = [f['id'] for f in before['fans']]
        for order in (None, ids[:-1], ids + [ids[0]], [ids[0]] * len(ids), [*ids[:-1], 'missing']):
            with self.subTest(order=order), self.assertRaises(ValueError):
                self.engine.reorder_fans(order)
            self.assertEqual(self.engine.config, before)
        with self.assertRaises(ConfigConflict):
            self.engine.reorder_fans(list(reversed(ids)), expected=list(reversed(ids)))
        self.assertEqual(self.engine.config, before)

    def test_stale_fan_curve_and_import_edits_cannot_overwrite_newer_changes(self):
        original = self.engine.config['fans'][0]['name']
        self.engine.patch_fan(self.first, {'name': '新名称'})
        with self.assertRaises(ConfigConflict):
            self.engine.patch_fan(self.first, {'name': '旧编辑器'}, {'name': original})
        revision = self.engine.config_revision()
        id_ = self.engine.save_curve({'name': '曲线', 'points': [[20, 20], [80, 100]]})
        old = copy.deepcopy(self.engine.config['curves'][0])
        self.engine.save_curve({**old, 'name': '新版曲线'}, expected=old)
        with self.assertRaises(ConfigConflict):
            self.engine.save_curve({**old, 'name': '旧编辑器'}, expected=old)
        with self.assertRaises(ConfigConflict):
            self.engine.update_config(self.engine.config, expected_revision=revision)
        self.assertEqual(self.engine.config['curves'][0]['id'], id_)
        self.assertEqual(self.engine.config['curves'][0]['name'], '新版曲线')

    def test_create_and_assign_curve_is_atomic(self):
        base = self.engine.config['fans'][0]
        id_ = self.engine.save_curve({'name': '共享', 'points': [[20, 25], [80, 100]]},
                                     assign_to=self.first, expected_fan={'curve_id': base['curve_id']})
        self.assertEqual(self.engine.config['fans'][0]['curve_id'], id_)
        before = copy.deepcopy(self.engine.config)
        with self.assertRaises(ConfigConflict):
            self.engine.save_curve({'name': '不会留下', 'points': [[20, 25], [80, 100]]},
                                   assign_to=self.first, expected_fan={'curve_id': None})
        self.assertEqual(self.engine.config, before)

    def test_used_curve_cannot_be_deleted_or_referenced_after_deletion(self):
        id_ = self.engine.save_curve({'name': '共享', 'points': [[20, 20], [80, 100]]})
        self.engine.patch_fan(self.first, {'curve_id': id_})
        with self.assertRaisesRegex(ValueError, '引用'):
            self.engine.delete_curve(id_)
        self.engine.patch_fan(self.first, {'curve_id': None})
        self.engine.delete_curve(id_)
        with self.assertRaisesRegex(ValueError, '不存在'):
            self.engine.patch_fan(self.first, {'curve_id': id_})

    def test_binding_change_requires_reconfirmation(self):
        config = copy.deepcopy(self.engine.config)
        config['fans'][0].update(channel_id=None, autostart=True)
        self.engine.update_config(config)
        f = self.engine.config['fans'][0]
        self.assertFalse(f['identified'])
        self.assertFalse(f['autostart'])
        with self.assertRaises(ValueError):
            self.engine.patch_fan(self.first, {'identified': True})

    def test_failed_disk_save_keeps_existing_policy_and_lease(self):
        self.engine.activate(self.first)
        original = copy.deepcopy(self.engine.config)
        lease = self.engine.leases[self.first]
        with patch('fancontrollx.engine.save_config', side_effect=OSError('disk full')):
            with self.assertRaises(OSError):
                self.engine.patch_fan(self.first, {'mode': 'manual', 'manual_duty': 80})
        self.assertEqual(self.engine.config, original)
        self.assertIs(self.engine.leases[self.first], lease)
        self.assertIn(self.first, self.engine.enabled)

    def test_no_source_is_inferred_from_channel_name(self):
        self.assertTrue(all(f['sensor_ids'] == ['@cpu'] for f in self.engine.config['fans']))
        c = self.engine.discovery()[0]
        for name in ['CPU Fan', 'GPU fan', 'Pump Fan', '任意设备']:
            self.assertEqual(self.engine.discovered_fan({**c, 'name': name})['sensor_ids'], ['@cpu'])

    def test_identification_timeout_resumes_without_losing_firmware_restore(self):
        self.engine.activate(self.first)
        lease = self.engine.leases[self.first]
        self.engine.identify('demo/pwm1', 70)
        self.engine.tick(now=self.engine.identification['expires_at'] + 1)
        self.assertIn(self.first, self.engine.enabled)
        self.assertIs(self.engine.leases[self.first], lease)
        self.engine.close()
        self.assertEqual(self.hw.duties['demo/pwm1'], 38)

    def test_stop_all_during_identification_never_resumes(self):
        self.engine.managed = True
        self.engine.request_control(self.first, True)
        self.engine.identify('demo/pwm1', 70)
        self.engine.request_release_all()
        self.engine.queue_autostart()
        self.engine.resume_controls()
        self.assertFalse(self.engine.enabled)
        self.assertFalse(self.engine.leases)
        self.assertEqual(self.hw.duties['demo/pwm1'], 38)
        self.assertFalse(self.engine.config['fans'][0]['autostart'])

    def test_manual_output_keeps_raw_temperature_protection(self):
        self.engine.patch_fan(self.first, {'mode': 'manual', 'manual_duty': 35, 'max_duty': 60})
        self.engine.activate(self.first)
        self.hw.cpu = 95
        self.engine.tick()
        self.assertEqual(self.hw.duties['demo/pwm1'], 100)

    def test_failed_stop_all_restoration_never_resumes_on_retry(self):
        self.engine.managed = True
        self.engine.request_control(self.first, True)
        self.engine.identify('demo/pwm1', 70)
        self.hw.fail_restore = True
        with self.assertRaises(OSError):
            self.engine.request_release_all()
        self.hw.fail_restore = False
        self.engine.tick()
        self.engine.resume_controls()
        self.assertFalse(self.engine.enabled)
        self.assertFalse(self.engine.leases)
        self.assertEqual(self.hw.duties['demo/pwm1'], 38)


class UpgradeTests(unittest.TestCase):
    def test_v1_is_lossless_on_two_independent_machines_and_backup_is_unchanged(self):
        with tempfile.TemporaryDirectory() as folder:
            for machine in ('machine-one', 'machine-two'):
                with self.subTest(machine=machine):
                    f = default_fan(machine, f'{machine} 风扇', '@gpu')
                    f.pop('curve_id')
                    f.update(channel_id='demo/pwm1', rpm_sensor_id='demo/fan1', identified=True,
                             device_kind='fan', autostart=True, points=[[30, 34], [73, 91]],
                             hysteresis=3, smoothing_s=11, manual_duty=63)
                    raw = {'version': 1, 'fans': [f]}
                    path = Path(folder) / machine / 'config.json'
                    save_config(path, raw)
                    engine = Engine(FixedHardware(), path, demo=True, managed=True)
                    try:
                        migrated = next(x for x in engine.config['fans'] if x['id'] == machine)
                        for key, value in f.items():
                            self.assertEqual(migrated[key], value)
                        self.assertEqual(json.loads(path.read_text()), raw, 'Loading must not write configuration')
                        engine.resume_controls()
                        self.assertIn(machine, engine.enabled)
                        engine.patch_fan(machine, {'name': f'{machine} 更新'})
                        backup = path.with_name('config.before-v2.json')
                        self.assertEqual(json.loads(backup.read_text()), raw)
                        engine.patch_fan(machine, {'manual_duty': 66})
                        self.assertEqual(json.loads(backup.read_text()), raw)
                    finally:
                        engine.close()
                    restarted = Engine(FixedHardware(), path, demo=True, managed=True)
                    try:
                        restarted.resume_controls()
                        migrated = next(x for x in restarted.config['fans'] if x['id'] == machine)
                        self.assertEqual(migrated['name'], f'{machine} 更新')
                        self.assertEqual(migrated['points'], f['points'])
                        self.assertEqual(migrated['sensor_ids'], ['@gpu'])
                        self.assertEqual(migrated['manual_duty'], 66)
                        self.assertIn(machine, restarted.enabled)
                    finally:
                        restarted.close()

    def test_invalid_shared_curves_and_ids_are_rejected(self):
        for curves in [[{'id':'x', 'name':'坏曲线', 'points':[[40,50],[40,80]]}],
                       [{'id':'x', 'name':'重复', 'points':[[20,20],[80,100]]}]*2]:
            with self.subTest(curves=curves), self.assertRaises(ValueError):
                validate_config({'version':2, 'fans':[], 'curves':curves})


# Reuse the HTTP fixture, without collecting its original test class twice.
class WorkspaceHTTPTests(unittest.TestCase):
    setUp = test_core.HTTPTests.setUp
    cleanup = test_core.HTTPTests.cleanup
    request = test_core.HTTPTests.request

    def test_card_order_endpoint_checks_token_ids_and_concurrent_order(self):
        ids = [f['id'] for f in self.engine.config['fans']]
        headers = {'Content-Type': 'application/json', 'X-FanControl-Token': self.server.csrf}
        body = {'ids': list(reversed(ids)), 'expected': ids}
        self.assertEqual(self.request('POST', '/api/fan-order', body, {'Content-Type': 'application/json'})[0], 403)
        self.assertEqual(self.request('POST', '/api/fan-order', body, headers)[0], 200)
        self.assertEqual(self.request('POST', '/api/fan-order', body, headers)[0], 409)
        self.assertEqual(self.request('POST', '/api/fan-order', {'ids': ids[:-1]}, headers)[0], 400)

    def test_scoped_fan_and_curve_endpoints_require_tokens_and_report_conflicts(self):
        headers = {'Content-Type': 'application/json', 'X-FanControl-Token': self.server.csrf}
        self.assertEqual(self.request('POST', '/api/fan', {'id':self.cpu_id, 'changes':{'manual_duty':77}},
                                      {'Content-Type':'application/json'})[0], 403)
        self.assertEqual(self.request('POST', '/api/fan', {'id':self.cpu_id, 'changes':{'manual_duty':77},
                                                          'expected':{'manual_duty':50}}, headers)[0], 200)
        self.assertEqual(self.request('POST', '/api/fan', {'id':self.cpu_id, 'changes':{'manual_duty':66},
                                                          'expected':{'manual_duty':50}}, headers)[0], 409)
        status, data = self.request('POST', '/api/curve', {'curve':{'name':'我的曲线', 'points':[[20,20],[80,100]]}}, headers)
        self.assertEqual(status, 200)
        id_ = json.loads(data)['id']
        self.assertEqual(self.request('POST', '/api/curve/delete', {'id':id_}, headers)[0], 200)
