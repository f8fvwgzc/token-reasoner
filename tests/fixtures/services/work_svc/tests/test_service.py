from work_svc.service import TaskService


def test_create_task():
    assert TaskService().create_task("Hello")
